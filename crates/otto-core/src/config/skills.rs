//! The `[skills]` table and its resolution.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::paths;

/// Used when `[skills].paths` is absent. A configured empty list means no
/// roots and is kept distinct via `Option`.
const DEFAULT_SKILLS_PATHS: [&str; 2] = ["~/.otto/skills", ".otto/skills"];

/// The `[skills]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skills {
    pub enabled: Option<bool>,
    pub paths: Option<Vec<String>>,
}

/// The resolved `[skills]` configuration for one runner build.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsRuntime {
    pub enabled: bool,
    /// Absolute, cleaned skill root directories in configured order. Later
    /// entries win on a name conflict during discovery.
    pub roots: Vec<String>,
}

/// Resolves `[skills]` into roots ready for skill discovery. Never errors: an
/// unresolvable `~/` entry (no home in `env`) is skipped.
pub fn resolve_skills(
    file: &super::File,
    env: &HashMap<String, String>,
    workspace_path: &str,
) -> SkillsRuntime {
    let enabled = file.skills.enabled.unwrap_or(true);
    if !enabled {
        return SkillsRuntime {
            enabled: false,
            roots: Vec::new(),
        };
    }

    let default_paths: Vec<String> = DEFAULT_SKILLS_PATHS.iter().map(|p| p.to_string()).collect();
    let configured = file.skills.paths.as_ref().unwrap_or(&default_paths);
    SkillsRuntime {
        enabled: true,
        roots: resolve_roots(configured, env, workspace_path),
    }
}

/// Expands each entry of `entries` into an absolute, cleaned directory: a
/// `~/` prefix resolves against `env`'s home (skipped when home is
/// unresolvable), a relative path joins against `workspace_path`, and an
/// absolute path is cleaned as-is. An empty entry is skipped. Shared by
/// [`resolve_skills`] and [`super::agents::resolve_agents`].
pub(super) fn resolve_roots(
    entries: &[String],
    env: &HashMap<String, String>,
    workspace_path: &str,
) -> Vec<String> {
    let home = paths::home_from_env(env);
    let mut roots = Vec::new();
    for entry in entries {
        if entry.is_empty() {
            continue;
        }
        if let Some(rest) = entry.strip_prefix("~/") {
            if home.is_empty() {
                continue;
            }
            roots.push(paths::join(home, rest));
        } else if paths::is_abs(entry) {
            roots.push(paths::clean(entry));
        } else {
            roots.push(paths::join(workspace_path, entry));
        }
    }
    roots
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
    fn defaults_to_home_and_workspace_dot_otto_skills() {
        let runtime = resolve_skills(&File::default(), &env("/home/u"), "/work");
        assert!(runtime.enabled);
        assert_eq!(
            runtime.roots,
            vec!["/home/u/.otto/skills", "/work/.otto/skills"]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn expands_home_relative_and_absolute_paths() {
        let mut file = File::default();
        file.skills.paths = Some(vec![
            "~/skills-a".into(),
            "relative/skills-b".into(),
            "/abs/skills-c".into(),
        ]);
        let runtime = resolve_skills(&file, &env("/home/u"), "/work");
        assert_eq!(
            runtime.roots,
            vec![
                "/home/u/skills-a",
                "/work/relative/skills-b",
                "/abs/skills-c"
            ]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn explicit_empty_paths_means_no_roots() {
        let mut file = File::default();
        file.skills.paths = Some(Vec::new());
        let runtime = resolve_skills(&file, &env("/home/u"), "/work");
        assert!(runtime.enabled);
        assert!(runtime.roots.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn disabled_yields_no_roots() {
        let mut file = File::default();
        file.skills.enabled = Some(false);
        let runtime = resolve_skills(&file, &env("/home/u"), "/work");
        assert!(!runtime.enabled);
        assert!(runtime.roots.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn skips_empty_entry_and_home_tilde_without_home() {
        let mut file = File::default();
        file.skills.paths = Some(vec![
            "".into(),
            "~/skills-a".into(),
            "relative/skills-b".into(),
        ]);
        let runtime = resolve_skills(&file, &env(""), "/work");
        assert_eq!(runtime.roots, vec!["/work/relative/skills-b"]);
    }
}
