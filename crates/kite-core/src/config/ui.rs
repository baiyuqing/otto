//! The `[ui]` table and its resolution.

use serde::{Deserialize, Serialize};

use super::ConfigError;

/// The `[ui]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    #[serde(default)]
    pub mode: String,
}

/// The resolved frontend a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Auto,
    Tui,
    Repl,
}

impl UiMode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "tui" => Some(Self::Tui),
            "repl" => Some(Self::Repl),
            _ => None,
        }
    }
}

/// Picks the UI mode: `override_value` (CLI flag) > `KITE_UI` (from `env`) >
/// `[ui].mode` in the file > [`UiMode::Auto`]. An unrecognized, non-empty
/// candidate at any of those levels is an error.
pub fn resolve_ui_mode(
    file: &super::File,
    env: &std::collections::HashMap<String, String>,
    override_value: &str,
) -> Result<UiMode, ConfigError> {
    let kite_ui = env.get("KITE_UI").map(String::as_str).unwrap_or("");
    for candidate in [override_value, kite_ui, file.ui.mode.as_str()] {
        if let Some(mode) = parse_ui_mode(candidate)? {
            return Ok(mode);
        }
    }
    Ok(UiMode::Auto)
}

fn parse_ui_mode(value: &str) -> Result<Option<UiMode>, ConfigError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match UiMode::parse(&trimmed.to_lowercase()) {
        Some(mode) => Ok(Some(mode)),
        None => Err(ConfigError::new(format!(
            "invalid ui mode \"{trimmed}\": must be one of auto, tui, repl"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::File;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn defaults_to_auto() {
        let mode = resolve_ui_mode(&File::default(), &HashMap::new(), "").expect("resolve");
        assert_eq!(mode, UiMode::Auto);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn override_wins_over_env_and_file() {
        let mut file = File::default();
        file.ui.mode = "tui".into();
        let mut env = HashMap::new();
        env.insert("KITE_UI".to_string(), "repl".to_string());
        let mode = resolve_ui_mode(&file, &env, "auto").expect("resolve");
        assert_eq!(mode, UiMode::Auto);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn env_wins_over_file() {
        let mut file = File::default();
        file.ui.mode = "tui".into();
        let mut env = HashMap::new();
        env.insert("KITE_UI".to_string(), "repl".to_string());
        let mode = resolve_ui_mode(&file, &env, "").expect("resolve");
        assert_eq!(mode, UiMode::Repl);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn file_is_used_last() {
        let mut file = File::default();
        file.ui.mode = "tui".into();
        let mode = resolve_ui_mode(&file, &HashMap::new(), "").expect("resolve");
        assert_eq!(mode, UiMode::Tui);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_mode() {
        let err = resolve_ui_mode(&File::default(), &HashMap::new(), "bogus").unwrap_err();
        assert!(err.to_string().contains("invalid ui mode"), "{err}");
    }
}
