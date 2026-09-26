//! The `[server]` table and its resolution.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{ConfigError, paths};

const DEFAULT_SERVER_SOCKET: &str = "~/.otto/otto.sock";

/// The `[server]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default)]
    pub socket: String,
    #[serde(default)]
    pub listen: String,
    /// Directories, besides the startup workspace, a client may open. Empty
    /// by default, which admits only the startup workspace.
    #[serde(default)]
    pub workspace_roots: Vec<String>,
}

/// The resolved `[server]` configuration for one process. Exactly one of
/// `listen` and `socket` is set: a non-empty `listen` means the server binds
/// a loopback TCP address and serves no Unix socket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerRuntime {
    /// Absolute path, when set.
    pub socket: String,
    /// `host:port`, when set.
    pub listen: String,
    /// `workspace_roots` with `~` expanded, in file order. Not yet
    /// canonicalized: `otto-core` stays wasm-safe and does no filesystem
    /// access, so the native caller resolves symlinks and checks each entry
    /// is an existing directory.
    pub workspace_roots: Vec<String>,
}

/// Picks the listener: `listen_override` (`--listen`) > `socket_override`
/// (`--socket`) > `file.server.listen` > `file.server.socket` >
/// [`DEFAULT_SERVER_SOCKET`]. A relative socket path is only cleaned, not
/// resolved against a directory; the native caller resolves it against the
/// process working directory.
pub fn resolve_server(
    file: &super::File,
    env: &HashMap<String, String>,
    socket_override: &str,
    listen_override: &str,
) -> Result<ServerRuntime, ConfigError> {
    let mut runtime = if !listen_override.is_empty() {
        ServerRuntime {
            listen: listen_override.to_string(),
            ..Default::default()
        }
    } else if !socket_override.is_empty() {
        resolve_socket(socket_override, env)?
    } else if !file.server.listen.is_empty() {
        ServerRuntime {
            listen: file.server.listen.clone(),
            ..Default::default()
        }
    } else if !file.server.socket.is_empty() {
        resolve_socket(&file.server.socket, env)?
    } else {
        resolve_socket(DEFAULT_SERVER_SOCKET, env)?
    };
    runtime.workspace_roots = file
        .server
        .workspace_roots
        .iter()
        .map(|root| expand_tilde(root, env, "workspace root"))
        .collect::<Result<_, _>>()?;
    Ok(runtime)
}

fn resolve_socket(
    socket: &str,
    env: &HashMap<String, String>,
) -> Result<ServerRuntime, ConfigError> {
    Ok(ServerRuntime {
        socket: expand_tilde(socket, env, "server socket")?,
        ..Default::default()
    })
}

/// Expands a leading `~/` against `env`'s home directory, like `--cwd` and
/// `[server].socket` both do; a plain path is only cleaned.
fn expand_tilde(
    path: &str,
    env: &HashMap<String, String>,
    what: &str,
) -> Result<String, ConfigError> {
    let Some(rest) = path.strip_prefix("~/") else {
        return Ok(paths::clean(path));
    };
    let home = paths::home_from_env(env);
    if home.is_empty() {
        return Err(ConfigError::new(format!(
            "resolve home directory for {what} \"{path}\""
        )));
    }
    Ok(paths::clean(&paths::join(home, rest)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::File;

    fn env(home: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), home.to_string());
        env
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn socket_override_wins_over_file() {
        let mut file = File::default();
        file.server.socket = "/file/otto.sock".into();
        let runtime =
            resolve_server(&file, &env("/home"), "/override/otto.sock", "").expect("resolve");
        assert_eq!(
            runtime,
            ServerRuntime {
                socket: "/override/otto.sock".into(),
                listen: String::new(),
                workspace_roots: Vec::new(),
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn file_wins_over_default() {
        let mut file = File::default();
        file.server.socket = "/file/otto.sock".into();
        let runtime = resolve_server(&file, &env("/home"), "", "").expect("resolve");
        assert_eq!(runtime.socket, "/file/otto.sock");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn default_expands_tilde_to_env_home() {
        let runtime = resolve_server(&File::default(), &env("/home"), "", "").expect("resolve");
        assert_eq!(runtime.socket, "/home/.otto/otto.sock");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn tilde_with_no_home_errors() {
        assert!(resolve_server(&File::default(), &HashMap::new(), "", "").is_err());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn plain_absolute_path_is_cleaned_unchanged() {
        let mut file = File::default();
        file.server.socket = "/x/y/../otto.sock".into();
        let runtime = resolve_server(&file, &env("/home"), "", "").expect("resolve");
        assert_eq!(runtime.socket, "/x/otto.sock");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn listen_override_wins_over_everything() {
        let mut file = File::default();
        file.server.socket = "/file/otto.sock".into();
        file.server.listen = "127.0.0.1:1".into();
        let runtime = resolve_server(&file, &env("/home"), "/override/otto.sock", "127.0.0.1:2")
            .expect("resolve");
        assert_eq!(
            runtime,
            ServerRuntime {
                socket: String::new(),
                listen: "127.0.0.1:2".into(),
                workspace_roots: Vec::new(),
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn socket_override_wins_over_file_listen() {
        let mut file = File::default();
        file.server.listen = "127.0.0.1:1".into();
        let runtime =
            resolve_server(&file, &env("/home"), "/override/otto.sock", "").expect("resolve");
        assert_eq!(
            runtime,
            ServerRuntime {
                socket: "/override/otto.sock".into(),
                listen: String::new(),
                workspace_roots: Vec::new(),
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn file_listen_wins_over_file_socket_and_leaves_socket_empty() {
        let mut file = File::default();
        file.server.socket = "/file/otto.sock".into();
        file.server.listen = "127.0.0.1:1".into();
        let runtime = resolve_server(&file, &env("/home"), "", "").expect("resolve");
        assert_eq!(
            runtime,
            ServerRuntime {
                socket: String::new(),
                listen: "127.0.0.1:1".into(),
                workspace_roots: Vec::new(),
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn file_listen_needs_no_home() {
        let mut file = File::default();
        file.server.listen = "127.0.0.1:1".into();
        let runtime = resolve_server(&file, &HashMap::new(), "", "").expect("resolve");
        assert_eq!(runtime.listen, "127.0.0.1:1");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_fields() {
        let err = super::super::parse("[server]\nsocket = \"/x/otto.sock\"\nunknown = true\n")
            .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn workspace_roots_default_to_empty() {
        assert!(File::default().server.workspace_roots.is_empty());
        let runtime = resolve_server(&File::default(), &env("/home"), "", "").expect("resolve");
        assert!(runtime.workspace_roots.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn workspace_roots_parse_from_toml() {
        let file = super::super::parse("[server]\nworkspace_roots = [\"/a\", \"~/Work\"]\n")
            .expect("parse");
        assert_eq!(
            file.server.workspace_roots,
            vec!["/a".to_string(), "~/Work".to_string()]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn workspace_roots_expand_tilde_like_the_socket() {
        let mut file = File::default();
        file.server.workspace_roots = vec!["/a".into(), "~/Work".into()];
        let runtime = resolve_server(&file, &env("/home"), "", "").expect("resolve");
        assert_eq!(
            runtime.workspace_roots,
            vec!["/a".to_string(), "/home/Work".to_string()]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn workspace_roots_tilde_with_no_home_errors() {
        let mut file = File::default();
        file.server.workspace_roots = vec!["~/Work".into()];
        assert!(resolve_server(&file, &HashMap::new(), "", "").is_err());
    }
}
