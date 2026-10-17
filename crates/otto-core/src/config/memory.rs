//! The `[memory]` table and its resolution.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{ConfigError, duration::parse_go_duration, paths};

const DEFAULT_MEMORY_BACKEND: &str = "files";
const DEFAULT_MEMORY_RECALL_TOKENS: i64 = 2000;
const DEFAULT_MEMORY_MAX_RESULTS: i64 = 12;
const REMOTE_MEMORY_BACKEND: &str = "remote";
const DEFAULT_REMOTE_TOKEN_ENV: &str = "OTTO_MEM_TOKEN";
const DEFAULT_REMOTE_RECALL_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REMOTE_RECALL_TIMEOUT: Duration = Duration::from_secs(10);
/// The scope id limit of `otto_core::memory::Scope`.
const MAX_REMOTE_USER_BYTES: usize = 128;

/// The recall budget and page size a request may ask for. Hardcoded rather
/// than imported from `otto::memory` because otto-core cannot depend on the
/// native crate.
const MAX_TOKEN_BUDGET: i64 = 8192;
const MAX_RECALL_RECORDS: i64 = 64;

/// The `[memory]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    pub enabled: Option<bool>,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub required: bool,
    pub recall_tokens: Option<i64>,
    pub max_results: Option<i64>,
    #[serde(default)]
    pub require_encryption: bool,
    #[serde(default)]
    pub workspace_ids: HashMap<String, String>,
    #[serde(default)]
    #[serde(alias = "sqlite")]
    pub turso: MemoryTurso,
    #[serde(default)]
    pub remote: MemoryRemote,
}

/// The `[memory.remote]` table, read only when `backend = "remote"`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRemote {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub token_env: String,
    #[serde(default)]
    pub recall_timeout: String,
}

impl Memory {
    /// The environment variable that holds the remote memory token, when
    /// `backend = "remote"`. The token is a secret like a provider key.
    pub fn credential_variable(&self) -> Option<&str> {
        (self.backend == REMOTE_MEMORY_BACKEND).then(|| self.remote.token_env())
    }
}

impl MemoryRemote {
    /// `token_env`, or `OTTO_MEM_TOKEN` when unset.
    pub fn token_env(&self) -> &str {
        if self.token_env.is_empty() {
            DEFAULT_REMOTE_TOKEN_ENV
        } else {
            &self.token_env
        }
    }
}

/// The `[memory.turso]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryTurso {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub busy_timeout: String,
}

/// The resolved `[memory]` configuration for one process.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryRuntime {
    pub enabled: bool,
    pub backend: String,
    pub required: bool,
    pub recall_tokens: i64,
    pub max_results: i64,
    pub require_encryption: bool,
    pub workspace_ids: HashMap<String, String>,
    /// Absolute `{home}/.otto/memory`; not configurable.
    pub dir: String,
    /// Absolute path of the Turso database that the first start imports.
    pub turso_path: String,
    /// The writer lock wait. Zero when `[memory.turso].busy_timeout` is absent.
    pub turso_busy_timeout: Duration,
    /// Set exactly when `backend` is `remote`.
    pub remote: Option<RemoteRuntime>,
}

/// The resolved `[memory.remote]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteRuntime {
    /// The service base URL without a trailing slash.
    pub url: String,
    /// The user scope id shared by every host of this user.
    pub user: String,
    /// The environment variable that holds the bearer token.
    pub token_env: String,
    pub recall_timeout: Duration,
}

/// Resolves `[memory]`. The store directory is `$HOME/.otto/memory`; the
/// Turso path, defaulting to `$HOME/.otto/memory/memory.db`, only locates the
/// database that the first start imports.
///
/// Add it back if a real memory override is introduced.
pub fn resolve_memory(
    file: &super::File,
    env: &HashMap<String, String>,
) -> Result<MemoryRuntime, ConfigError> {
    let enabled = file.memory.enabled.unwrap_or(true);

    let backend = if file.memory.backend.is_empty() {
        DEFAULT_MEMORY_BACKEND.to_string()
    } else {
        file.memory.backend.clone()
    };
    // Earlier releases selected the Turso engine with `turso` or `sqlite`; all
    // three values select the file store.
    let remote = match backend.as_str() {
        "files" | "turso" | "sqlite" => None,
        REMOTE_MEMORY_BACKEND => Some(resolve_remote(&file.memory.remote)?),
        _ => {
            return Err(ConfigError::new(format!(
                "unsupported memory backend \"{backend}\": must be files or remote"
            )));
        }
    };
    if remote.is_some() && file.memory.require_encryption {
        return Err(ConfigError::new(
            "memory require_encryption cannot be verified for the remote backend",
        ));
    }
    let backend = if remote.is_some() {
        REMOTE_MEMORY_BACKEND
    } else {
        DEFAULT_MEMORY_BACKEND
    }
    .to_string();

    let recall_tokens = match file.memory.recall_tokens {
        Some(value) if value <= 0 => {
            return Err(ConfigError::new(
                "invalid memory recall_tokens: must be greater than zero",
            ));
        }
        Some(value) if value > MAX_TOKEN_BUDGET => {
            return Err(ConfigError::new(format!(
                "invalid memory recall_tokens: must be at most {MAX_TOKEN_BUDGET}"
            )));
        }
        Some(value) => value,
        // Remote rendering spends up to 142 bytes of markup per item, which
        // leaves 165 bytes of text for 12 items at 2000.
        None if remote.is_some() => MAX_TOKEN_BUDGET,
        None => DEFAULT_MEMORY_RECALL_TOKENS,
    };

    let max_results = match file.memory.max_results {
        Some(value) if value <= 0 => {
            return Err(ConfigError::new(
                "invalid memory max_results: must be greater than zero",
            ));
        }
        Some(value) if value > MAX_RECALL_RECORDS => {
            return Err(ConfigError::new(format!(
                "invalid memory max_results: must be at most {MAX_RECALL_RECORDS}"
            )));
        }
        Some(value) => value,
        None => DEFAULT_MEMORY_MAX_RESULTS,
    };

    let home = paths::home_from_env(env);
    if home.is_empty() {
        return Err(ConfigError::new(
            "resolve home directory for memory directory: $HOME is not defined",
        ));
    }
    let dir = paths::clean(&format!("{home}/.otto/memory"));
    let turso_path = if !file.memory.turso.path.is_empty() {
        paths::expand_tilde(&file.memory.turso.path, env, "memory turso path")?
    } else {
        paths::clean(&format!("{home}/.otto/memory/memory.db"))
    };

    let mut turso_busy_timeout = Duration::ZERO;
    if !file.memory.turso.busy_timeout.is_empty() {
        let nanos = parse_go_duration(&file.memory.turso.busy_timeout)
            .map_err(|err| ConfigError::new(format!("invalid memory turso busy_timeout: {err}")))?;
        if nanos <= 0 {
            return Err(ConfigError::new(
                "invalid memory turso busy_timeout: must be greater than zero",
            ));
        }
        turso_busy_timeout = Duration::from_nanos(nanos as u64);
    }

    Ok(MemoryRuntime {
        enabled,
        backend,
        required: file.memory.required,
        recall_tokens,
        max_results,
        require_encryption: file.memory.require_encryption,
        workspace_ids: file.memory.workspace_ids.clone(),
        dir,
        turso_path,
        turso_busy_timeout,
        remote,
    })
}

fn resolve_remote(remote: &MemoryRemote) -> Result<RemoteRuntime, ConfigError> {
    let url = remote.url.trim_end_matches('/');
    let invalid = |reason: &str| ConfigError::new(format!("invalid memory remote url: {reason}"));
    if url.is_empty() {
        return Err(invalid("must be set"));
    }
    let parsed = url::Url::parse(url).map_err(|err| invalid(&err.to_string()))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid("must not contain credentials"));
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(invalid("must not have a path, query or fragment"));
    }
    let loopback = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => return Err(invalid("must have a host")),
    };
    match parsed.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => return Err(invalid("http is allowed only for a loopback host")),
        _ => return Err(invalid("must use https")),
    }

    let user = remote.user.as_str();
    if user.is_empty()
        || user.len() > MAX_REMOTE_USER_BYTES
        || !user.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(ConfigError::new(format!(
            "invalid memory remote user: must be 1 to {MAX_REMOTE_USER_BYTES} printable ASCII bytes"
        )));
    }

    if !super::storage::environment_name(remote.token_env()) || remote.token_env().len() > 128 {
        return Err(ConfigError::new(
            "invalid memory remote token_env: must be an environment variable name",
        ));
    }

    let recall_timeout = if remote.recall_timeout.is_empty() {
        DEFAULT_REMOTE_RECALL_TIMEOUT
    } else {
        let nanos = parse_go_duration(&remote.recall_timeout).map_err(|err| {
            ConfigError::new(format!("invalid memory remote recall_timeout: {err}"))
        })?;
        let timeout = Duration::from_nanos(nanos.max(0) as u64);
        if timeout.is_zero() || timeout > MAX_REMOTE_RECALL_TIMEOUT {
            return Err(ConfigError::new(
                "invalid memory remote recall_timeout: must be greater than zero and at most 10s",
            ));
        }
        timeout
    };

    Ok(RemoteRuntime {
        url: url.to_string(),
        user: user.to_string(),
        token_env: remote.token_env().to_string(),
        recall_timeout,
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
    fn resolves_defaults() {
        let runtime = resolve_memory(&File::default(), &env("/home/u")).expect("resolve");
        assert!(runtime.enabled);
        assert_eq!(runtime.backend, "files");
        assert_eq!(runtime.dir, "/home/u/.otto/memory");
        assert_eq!(runtime.recall_tokens, 2000);
        assert_eq!(runtime.max_results, 12);
        assert_eq!(runtime.turso_path, "/home/u/.otto/memory/memory.db");
        assert_eq!(runtime.turso_busy_timeout, Duration::ZERO);
        assert!(runtime.workspace_ids.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn legacy_configuration_resolves_to_files() {
        let file: File = toml::from_str(
            r#"
[memory]
backend = "sqlite"
[memory.sqlite]
path = "/custom/memory.db"
busy_timeout = "10s"
"#,
        )
        .expect("legacy config");
        let runtime = resolve_memory(&file, &env("/home/u")).expect("resolve");
        assert_eq!(runtime.backend, "files");
        assert_eq!(runtime.dir, "/home/u/.otto/memory");
        assert_eq!(runtime.turso_path, "/custom/memory.db");
        assert_eq!(runtime.turso_busy_timeout, Duration::from_secs(10));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn a_home_relative_turso_path_resolves_against_home_not_the_working_directory() {
        let mut file = File::default();
        file.memory.turso.path = "~/.otto/memory/memory-turso.db".into();
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), "/home/u".to_string());
        let runtime = resolve_memory(&file, &env).expect("resolve");
        assert_eq!(runtime.turso_path, "/home/u/.otto/memory/memory-turso.db");

        let err = resolve_memory(&file, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("home"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn files_turso_and_sqlite_select_the_file_store() {
        for name in ["files", "turso", "sqlite"] {
            let mut file = File::default();
            file.memory.backend = name.into();
            let runtime = resolve_memory(&file, &env("/home/u")).expect("resolve");
            assert_eq!(runtime.backend, "files", "{name}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unresolvable_home() {
        let err = resolve_memory(&File::default(), &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("home"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn applies_toml_overrides() {
        let mut file = File::default();
        file.memory.enabled = Some(false);
        file.memory.backend = "files".into();
        file.memory.required = true;
        file.memory.recall_tokens = Some(500);
        file.memory.max_results = Some(3);
        file.memory.require_encryption = true;
        file.memory
            .workspace_ids
            .insert("/old/path".into(), "stable-id".into());
        file.memory.turso = MemoryTurso {
            path: "/custom/memory.db".into(),
            busy_timeout: "10s".into(),
        };

        let runtime = resolve_memory(&file, &env("/home/u")).expect("resolve");
        assert!(!runtime.enabled);
        assert!(runtime.required);
        assert_eq!(runtime.recall_tokens, 500);
        assert_eq!(runtime.max_results, 3);
        assert!(runtime.require_encryption);
        assert_eq!(
            runtime.workspace_ids.get("/old/path"),
            Some(&"stable-id".to_string())
        );
        assert_eq!(runtime.turso_path, "/custom/memory.db");
        assert_eq!(runtime.turso_busy_timeout, Duration::from_secs(10));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unsupported_backend() {
        let mut file = File::default();
        file.memory.backend = "postgres".into();
        let err = resolve_memory(&file, &env("/home/u")).unwrap_err();
        assert!(err.to_string().contains("postgres"), "{err}");
        assert!(err.to_string().contains("must be files or remote"), "{err}");
    }

    fn remote_file(remote: &str) -> File {
        toml::from_str(&format!(
            "[memory]\nbackend = \"remote\"\n[memory.remote]\n{remote}"
        ))
        .expect("remote config")
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn resolves_remote_defaults() {
        let file = remote_file("url = \"https://mem.example:8443/\"\nuser = \"alice\"\n");
        let runtime = resolve_memory(&file, &env("/home/u")).expect("resolve");
        assert_eq!(runtime.backend, "remote");
        assert_eq!(runtime.recall_tokens, MAX_TOKEN_BUDGET);
        assert_eq!(
            runtime.remote,
            Some(RemoteRuntime {
                url: "https://mem.example:8443".into(),
                user: "alice".into(),
                token_env: "OTTO_MEM_TOKEN".into(),
                recall_timeout: Duration::from_secs(3),
            })
        );
        assert_eq!(file.memory.credential_variable(), Some("OTTO_MEM_TOKEN"));
        let files = resolve_memory(&File::default(), &env("/home/u")).expect("files");
        assert_eq!(File::default().memory.credential_variable(), None);
        assert_eq!(files.remote, None);
        assert_eq!(files.recall_tokens, DEFAULT_MEMORY_RECALL_TOKENS);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn remote_keeps_explicit_settings() {
        let file = remote_file(
            "url = \"http://127.0.0.1:9000\"\nuser = \"alice\"\ntoken_env = \"MEM\"\nrecall_timeout = \"500ms\"\n",
        );
        let mut file = file;
        file.memory.recall_tokens = Some(3000);
        let runtime = resolve_memory(&file, &env("/home/u")).expect("resolve");
        let remote = runtime.remote.expect("remote");
        assert_eq!(remote.token_env, "MEM");
        assert_eq!(file.memory.credential_variable(), Some("MEM"));
        assert_eq!(remote.recall_timeout, Duration::from_millis(500));
        assert_eq!(runtime.recall_tokens, 3000);
        for url in ["http://localhost:9000", "http://[::1]:9000"] {
            let file = remote_file(&format!("url = \"{url}\"\nuser = \"alice\"\n"));
            resolve_memory(&file, &env("/home/u")).expect(url);
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_remote_settings() {
        for (remote, expected) in [
            ("user = \"alice\"\n", "url"),
            (
                "url = \"http://mem.example\"\nuser = \"alice\"\n",
                "loopback",
            ),
            ("url = \"ftp://mem.example\"\nuser = \"alice\"\n", "https"),
            (
                "url = \"https://mem.example/v1\"\nuser = \"alice\"\n",
                "path",
            ),
            (
                "url = \"https://mem.example?a=1\"\nuser = \"alice\"\n",
                "query",
            ),
            (
                "url = \"https://u:p@mem.example\"\nuser = \"alice\"\n",
                "credentials",
            ),
            ("url = \"https://mem.example\"\n", "user"),
            ("url = \"https://mem.example\"\nuser = \"a b\"\n", "user"),
            (
                "url = \"https://mem.example\"\nuser = \"alice\"\nrecall_timeout = \"11s\"\n",
                "recall_timeout",
            ),
            (
                "url = \"https://mem.example\"\nuser = \"alice\"\nrecall_timeout = \"0s\"\n",
                "recall_timeout",
            ),
            (
                "url = \"https://mem.example\"\nuser = \"alice\"\ntoken_env = \"A-B\"\n",
                "token_env",
            ),
        ] {
            let err = resolve_memory(&remote_file(remote), &env("/home/u")).unwrap_err();
            assert!(err.to_string().contains(expected), "{remote}: {err}");
        }
        let mut file = remote_file("url = \"https://mem.example\"\nuser = \"alice\"\n");
        file.memory.require_encryption = true;
        let err = resolve_memory(&file, &env("/home/u")).unwrap_err();
        assert!(err.to_string().contains("require_encryption"), "{err}");
        assert!(toml::from_str::<File>("[memory.remote]\nunknown = 1\n").is_err());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_non_positive_recall_tokens() {
        let mut file = File::default();
        file.memory.recall_tokens = Some(0);
        let err = resolve_memory(&file, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("recall_tokens"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_non_positive_max_results() {
        let mut file = File::default();
        file.memory.max_results = Some(-1);
        let err = resolve_memory(&file, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("max_results"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_recall_tokens_above_ceiling() {
        let mut file = File::default();
        file.memory.recall_tokens = Some(MAX_TOKEN_BUDGET + 1);
        let err = resolve_memory(&file, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("recall_tokens"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_max_results_above_ceiling() {
        let mut file = File::default();
        file.memory.max_results = Some(MAX_RECALL_RECORDS + 1);
        let err = resolve_memory(&file, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("max_results"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_busy_timeout() {
        // ponytail: core's `home_from_env` deliberately has no fallback to the
        // real environment (core must never touch real env), so this test
        // injects HOME itself to reach the busy_timeout validation below.
        let mut file = File::default();
        file.memory.turso.busy_timeout = "not-a-duration".into();
        let err = resolve_memory(&file, &env("/home/u")).unwrap_err();
        assert!(err.to_string().contains("busy_timeout"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_non_positive_busy_timeout() {
        // ponytail: see rejects_invalid_busy_timeout above for why HOME is
        // injected rather than passing an empty env.
        let mut file = File::default();
        file.memory.turso.busy_timeout = "0s".into();
        let err = resolve_memory(&file, &env("/home/u")).unwrap_err();
        assert!(err.to_string().contains("busy_timeout"), "{err}");
    }
}
