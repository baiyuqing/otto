//! The `[memory]` table and its resolution.

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{ConfigError, duration::parse_go_duration, paths};

const DEFAULT_MEMORY_BACKEND: &str = "sqlite";
const DEFAULT_MEMORY_RECALL_TOKENS: i64 = 2000;
const DEFAULT_MEMORY_MAX_RESULTS: i64 = 12;

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
    pub sqlite: MemorySQLite,
}

/// The `[memory.sqlite]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySQLite {
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
    /// Absolute path.
    pub sqlite_path: String,
    /// Zero when `[memory.sqlite].busy_timeout` is absent.
    pub sqlite_busy_timeout: Duration,
}

/// Resolves `[memory]`, defaulting the SQLite path to
/// `$HOME/.otto/memory/memory.db`.
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
    if backend != "sqlite" {
        return Err(ConfigError::new(format!(
            "unsupported memory backend \"{backend}\": must be sqlite"
        )));
    }

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

    let sqlite_path = if !file.memory.sqlite.path.is_empty() {
        file.memory.sqlite.path.clone()
    } else {
        let home = paths::home_from_env(env);
        if home.is_empty() {
            return Err(ConfigError::new(
                "resolve home directory for default memory sqlite path: $HOME is not defined",
            ));
        }
        paths::clean(&format!("{home}/.otto/memory/memory.db"))
    };

    let mut sqlite_busy_timeout = Duration::ZERO;
    if !file.memory.sqlite.busy_timeout.is_empty() {
        let nanos = parse_go_duration(&file.memory.sqlite.busy_timeout).map_err(|err| {
            ConfigError::new(format!("invalid memory sqlite busy_timeout: {err}"))
        })?;
        if nanos <= 0 {
            return Err(ConfigError::new(
                "invalid memory sqlite busy_timeout: must be greater than zero",
            ));
        }
        sqlite_busy_timeout = Duration::from_nanos(nanos as u64);
    }

    Ok(MemoryRuntime {
        enabled,
        backend,
        required: file.memory.required,
        recall_tokens,
        max_results,
        require_encryption: file.memory.require_encryption,
        workspace_ids: file.memory.workspace_ids.clone(),
        sqlite_path,
        sqlite_busy_timeout,
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
        assert_eq!(runtime.backend, "sqlite");
        assert_eq!(runtime.recall_tokens, 2000);
        assert_eq!(runtime.max_results, 12);
        assert_eq!(runtime.sqlite_path, "/home/u/.otto/memory/memory.db");
        assert_eq!(runtime.sqlite_busy_timeout, Duration::ZERO);
        assert!(runtime.workspace_ids.is_empty());
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
        file.memory.backend = "sqlite".into();
        file.memory.required = true;
        file.memory.recall_tokens = Some(500);
        file.memory.max_results = Some(3);
        file.memory.require_encryption = true;
        file.memory
            .workspace_ids
            .insert("/old/path".into(), "stable-id".into());
        file.memory.sqlite = MemorySQLite {
            path: "/custom/memory.db".into(),
            busy_timeout: "10s".into(),
        };

        let runtime = resolve_memory(&file, &HashMap::new()).expect("resolve");
        assert!(!runtime.enabled);
        assert!(runtime.required);
        assert_eq!(runtime.recall_tokens, 500);
        assert_eq!(runtime.max_results, 3);
        assert!(runtime.require_encryption);
        assert_eq!(
            runtime.workspace_ids.get("/old/path"),
            Some(&"stable-id".to_string())
        );
        assert_eq!(runtime.sqlite_path, "/custom/memory.db");
        assert_eq!(runtime.sqlite_busy_timeout, Duration::from_secs(10));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unsupported_backend() {
        let mut file = File::default();
        file.memory.backend = "postgres".into();
        let err = resolve_memory(&file, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("postgres"), "{err}");
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
        file.memory.sqlite.busy_timeout = "not-a-duration".into();
        let err = resolve_memory(&file, &env("/home/u")).unwrap_err();
        assert!(err.to_string().contains("busy_timeout"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_non_positive_busy_timeout() {
        // ponytail: see rejects_invalid_busy_timeout above for why HOME is
        // injected rather than passing an empty env.
        let mut file = File::default();
        file.memory.sqlite.busy_timeout = "0s".into();
        let err = resolve_memory(&file, &env("/home/u")).unwrap_err();
        assert!(err.to_string().contains("busy_timeout"), "{err}");
    }
}
