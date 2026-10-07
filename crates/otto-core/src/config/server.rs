//! The `[server]` table and its resolution.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{ConfigError, duration, paths};

const DEFAULT_SERVER_SOCKET: &str = "~/.otto/otto.sock";
/// How long a session with nothing running stays open, unless
/// `[server].session_idle_timeout` says otherwise.
pub const DEFAULT_SESSION_IDLE_TIMEOUT: &str = "30m";
/// How many sessions one `otto serve` process keeps open, unless
/// `[server].max_open_sessions` says otherwise.
pub const DEFAULT_MAX_OPEN_SESSIONS: i64 = 64;

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
    /// How long `otto serve` keeps a session that has nothing running, as a
    /// duration (`"30m"`, `"2h"`). Absent applies
    /// [`DEFAULT_SESSION_IDLE_TIMEOUT`]; `"0"` keeps idle sessions open until
    /// a client closes them.
    pub session_idle_timeout: Option<String>,
    /// How many sessions one `otto serve` process keeps open, closing the
    /// least recently used idle ones above it. Absent applies
    /// [`DEFAULT_MAX_OPEN_SESSIONS`]; `0` keeps every session open.
    pub max_open_sessions: Option<i64>,
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
    /// How long a session with nothing running stays open. `None` disables
    /// idle eviction: `otto serve` then keeps every session until a client
    /// closes it.
    pub session_idle_timeout: Option<Duration>,
    /// How many sessions one process keeps open, closing the least recently
    /// used idle ones above it. `None` means no cap.
    pub max_open_sessions: Option<usize>,
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
    if let Some(forced) = file
        .policy
        .as_ref()
        .and_then(|p| p.force.get("server"))
        .and_then(toml::Value::as_table)
    {
        if let Some(socket) = forced.get("socket").and_then(toml::Value::as_str) {
            let expected = if socket.is_empty() {
                String::new()
            } else {
                resolve_socket(socket, env)?.socket
            };
            if runtime.socket != expected {
                return Err(ConfigError::new(
                    "server socket is forced by managed policy",
                ));
            }
        }
        if forced
            .get("listen")
            .and_then(toml::Value::as_str)
            .is_some_and(|listen| runtime.listen != listen)
        {
            return Err(ConfigError::new(
                "server listener is forced by managed policy",
            ));
        }
    }
    runtime.workspace_roots = file
        .server
        .workspace_roots
        .iter()
        .map(|root| paths::expand_tilde(root, env, "workspace root"))
        .collect::<Result<_, _>>()?;
    runtime.session_idle_timeout =
        resolve_session_idle_timeout(file.server.session_idle_timeout.as_deref())?;
    runtime.max_open_sessions = resolve_max_open_sessions(file.server.max_open_sessions)?;
    Ok(runtime)
}

/// The idle window `otto serve` applies. An absent setting is
/// [`DEFAULT_SESSION_IDLE_TIMEOUT`]; `"0"` turns idle eviction off.
fn resolve_session_idle_timeout(configured: Option<&str>) -> Result<Option<Duration>, ConfigError> {
    let configured = configured.unwrap_or(DEFAULT_SESSION_IDLE_TIMEOUT);
    let nanos = duration::parse_go_duration(configured)
        .map_err(|error| ConfigError::new(format!("invalid session_idle_timeout: {error}")))?;
    if nanos < 0 {
        return Err(ConfigError::new(
            "invalid session_idle_timeout: must not be negative",
        ));
    }
    Ok(u64::try_from(nanos)
        .ok()
        .filter(|nanos| *nanos > 0)
        .map(Duration::from_nanos))
}

/// The open-session cap `otto serve` applies. An absent setting is
/// [`DEFAULT_MAX_OPEN_SESSIONS`]; `0` removes the cap.
fn resolve_max_open_sessions(configured: Option<i64>) -> Result<Option<usize>, ConfigError> {
    match configured.unwrap_or(DEFAULT_MAX_OPEN_SESSIONS) {
        0 => Ok(None),
        cap if cap < 0 => Err(ConfigError::new(
            "invalid max_open_sessions: must not be negative",
        )),
        cap => Ok(Some(cap as usize)),
    }
}

fn resolve_socket(
    socket: &str,
    env: &HashMap<String, String>,
) -> Result<ServerRuntime, ConfigError> {
    Ok(ServerRuntime {
        socket: paths::expand_tilde(socket, env, "server socket")?,
        ..Default::default()
    })
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
                session_idle_timeout: Some(Duration::from_secs(30 * 60)),
                max_open_sessions: Some(64),
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
                session_idle_timeout: Some(Duration::from_secs(30 * 60)),
                max_open_sessions: Some(64),
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
                session_idle_timeout: Some(Duration::from_secs(30 * 60)),
                max_open_sessions: Some(64),
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
                session_idle_timeout: Some(Duration::from_secs(30 * 60)),
                max_open_sessions: Some(64),
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

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn session_lifetime_defaults_are_on() {
        assert_eq!(File::default().server.session_idle_timeout, None);
        assert_eq!(File::default().server.max_open_sessions, None);
        let runtime = resolve_server(&File::default(), &env("/home"), "", "").expect("resolve");
        assert_eq!(
            runtime.session_idle_timeout,
            Some(Duration::from_secs(30 * 60))
        );
        assert_eq!(runtime.max_open_sessions, Some(64));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn session_lifetime_settings_parse_from_toml() {
        let file =
            super::super::parse("[server]\nsession_idle_timeout = \"2h\"\nmax_open_sessions = 8\n")
                .expect("parse");
        let runtime = resolve_server(&file, &env("/home"), "", "").expect("resolve");
        assert_eq!(
            runtime.session_idle_timeout,
            Some(Duration::from_secs(2 * 60 * 60))
        );
        assert_eq!(runtime.max_open_sessions, Some(8));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn zero_turns_one_session_lifetime_limit_off_at_a_time() {
        let mut file = File::default();
        file.server.session_idle_timeout = Some("0".into());
        let runtime = resolve_server(&file, &env("/home"), "", "").expect("resolve");
        assert_eq!(runtime.session_idle_timeout, None);
        assert_eq!(runtime.max_open_sessions, Some(64));

        file.server.session_idle_timeout = None;
        file.server.max_open_sessions = Some(0);
        let runtime = resolve_server(&file, &env("/home"), "", "").expect("resolve");
        assert_eq!(
            runtime.session_idle_timeout,
            Some(Duration::from_secs(30 * 60))
        );
        assert_eq!(runtime.max_open_sessions, None);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn session_lifetime_settings_reject_bad_values() {
        for (timeout, cap) in [
            (Some("30 minutes"), None),
            (Some(""), None),
            (Some("-5m"), None),
            (None, Some(-1)),
        ] {
            let mut file = File::default();
            file.server.session_idle_timeout = timeout.map(str::to_string);
            file.server.max_open_sessions = cap;
            let error = resolve_server(&file, &env("/home"), "", "").unwrap_err();
            let field = if timeout.is_some() {
                "session_idle_timeout"
            } else {
                "max_open_sessions"
            };
            assert!(error.to_string().contains(field), "{error}");
        }
    }
    #[test]
    fn managed_socket_cannot_be_shadowed_by_a_listen_flag_or_lower_layer() {
        let mut file = super::super::parse("[server]\nlisten='127.0.0.1:4321'").unwrap();
        let policy: super::super::layered::ManagedPolicy =
            toml::from_str("[force.server]\nsocket='/managed/otto.sock'").unwrap();
        file.server.socket = "/managed/otto.sock".into();
        file.policy = Some(policy);
        assert!(resolve_server(&file, &HashMap::new(), "", "").is_err());
        file.server.listen.clear();
        assert!(resolve_server(&file, &HashMap::new(), "", "").is_ok());
        assert!(resolve_server(&file, &HashMap::new(), "", "127.0.0.1:4321").is_err());
        assert!(resolve_server(&file, &HashMap::new(), "/other.sock", "").is_err());
    }
}
