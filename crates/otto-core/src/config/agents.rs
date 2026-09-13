//! The `[agents]` table and its resolution. Port of `internal/config/agents.go`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{ConfigError, skills::resolve_roots};

/// Used when `[agents].paths` is absent.
const DEFAULT_AGENTS_PATHS: [&str; 2] = ["~/.otto/agents", ".otto/agents"];

/// Default `[agents].max_parallel` when absent.
pub const DEFAULT_AGENTS_MAX_PARALLEL: i64 = 4;

/// The `[agents]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agents {
    pub enabled: Option<bool>,
    pub paths: Option<Vec<String>>,
    pub max_parallel: Option<i64>,
}

/// The resolved `[agents]` configuration for one runner build.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentsRuntime {
    pub enabled: bool,
    /// Absolute, cleaned agent root directories in configured order. Later
    /// entries win on a name conflict during discovery.
    pub roots: Vec<String>,
    /// 1..16, default 4.
    pub max_parallel: i64,
}

/// Resolves `[agents]` into roots ready for subagent discovery.
/// `max_parallel` is validated before the enabled short-circuit, so an
/// out-of-range value is always reported even when agents are disabled. An
/// unresolvable `~/` entry (no home in `env`) is skipped, same as
/// [`super::resolve_skills`].
pub fn resolve_agents(
    file: &super::File,
    env: &HashMap<String, String>,
    workspace_path: &str,
) -> Result<AgentsRuntime, ConfigError> {
    let max_parallel = file
        .agents
        .max_parallel
        .unwrap_or(DEFAULT_AGENTS_MAX_PARALLEL);
    if !(1..=16).contains(&max_parallel) {
        return Err(ConfigError::new(format!(
            "[agents].max_parallel must be between 1 and 16, got {max_parallel}"
        )));
    }

    let enabled = file.agents.enabled.unwrap_or(true);
    if !enabled {
        return Ok(AgentsRuntime {
            enabled: false,
            roots: Vec::new(),
            max_parallel,
        });
    }

    let default_paths: Vec<String> = DEFAULT_AGENTS_PATHS.iter().map(|p| p.to_string()).collect();
    let configured = file.agents.paths.as_ref().unwrap_or(&default_paths);
    Ok(AgentsRuntime {
        enabled: true,
        roots: resolve_roots(configured, env, workspace_path),
        max_parallel,
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
    fn defaults_to_home_and_workspace_dot_otto_agents() {
        let runtime = resolve_agents(&File::default(), &env("/home/u"), "/work").expect("resolve");
        assert!(runtime.enabled);
        assert_eq!(
            runtime.roots,
            vec!["/home/u/.otto/agents", "/work/.otto/agents"]
        );
        assert_eq!(runtime.max_parallel, DEFAULT_AGENTS_MAX_PARALLEL);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn disabled_yields_no_roots_but_keeps_max_parallel() {
        let mut file = File::default();
        file.agents.enabled = Some(false);
        let runtime = resolve_agents(&file, &env("/home/u"), "/work").expect("resolve");
        assert!(!runtime.enabled);
        assert!(runtime.roots.is_empty());
        assert_eq!(runtime.max_parallel, DEFAULT_AGENTS_MAX_PARALLEL);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn explicit_empty_paths_means_no_roots() {
        let mut file = File::default();
        file.agents.paths = Some(Vec::new());
        let runtime = resolve_agents(&file, &env("/home/u"), "/work").expect("resolve");
        assert!(runtime.enabled);
        assert!(runtime.roots.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn expands_home_relative_and_absolute_paths() {
        let mut file = File::default();
        file.agents.paths = Some(vec![
            "~/agents-a".into(),
            "relative/agents-b".into(),
            "/abs/agents-c".into(),
        ]);
        let runtime = resolve_agents(&file, &env("/home/u"), "/work").expect("resolve");
        assert_eq!(
            runtime.roots,
            vec![
                "/home/u/agents-a",
                "/work/relative/agents-b",
                "/abs/agents-c"
            ]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn skips_home_tilde_without_home() {
        let mut file = File::default();
        file.agents.paths = Some(vec![
            "".into(),
            "~/agents-a".into(),
            "relative/agents-b".into(),
        ]);
        let runtime = resolve_agents(&file, &env(""), "/work").expect("resolve");
        assert_eq!(runtime.roots, vec!["/work/relative/agents-b"]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn max_parallel_boundaries_accepted() {
        for value in [1_i64, 16] {
            let mut file = File::default();
            file.agents.max_parallel = Some(value);
            let runtime = resolve_agents(&file, &env("/home/u"), "/work").expect("resolve");
            assert_eq!(runtime.max_parallel, value);
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn max_parallel_out_of_range_rejected() {
        for value in [0_i64, 17] {
            let mut file = File::default();
            file.agents.max_parallel = Some(value);
            assert!(resolve_agents(&file, &env("/home/u"), "/work").is_err());
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn max_parallel_error_message_matches_go() {
        let mut file = File::default();
        file.agents.max_parallel = Some(0);
        let err = resolve_agents(&file, &env("/home/u"), "/work").unwrap_err();
        assert_eq!(
            err.to_string(),
            "[agents].max_parallel must be between 1 and 16, got 0"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn max_parallel_rejected_even_when_disabled() {
        let mut file = File::default();
        file.agents.enabled = Some(false);
        file.agents.max_parallel = Some(17);
        let err = resolve_agents(&file, &env("/home/u"), "/work").unwrap_err();
        assert_eq!(
            err.to_string(),
            "[agents].max_parallel must be between 1 and 16, got 17"
        );
    }
}
