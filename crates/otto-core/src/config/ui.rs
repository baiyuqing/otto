//! The `[ui]` table and its resolution.

use serde::{Deserialize, Serialize};

use super::ConfigError;

/// The `[ui]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    #[serde(default)]
    pub mode: String,
    /// Client-side keys for the read-only sub-agent observer.
    #[serde(default)]
    pub observer: ObserverBindings,
}

/// Observer key names are parsed by the native TUI before entering raw mode.
/// Empty strings disable a binding; omitted fields retain their defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObserverBindings {
    pub toggle: String,
    pub open: String,
    pub back: String,
}

impl Default for ObserverBindings {
    fn default() -> Self {
        Self {
            toggle: "F6".into(),
            open: String::new(),
            back: "Esc".into(),
        }
    }
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

/// Picks the UI mode: `override_value` (CLI flag) > `OTTO_UI` (from `env`) >
/// `[ui].mode` in the file > [`UiMode::Auto`]. An unrecognized, non-empty
/// candidate at any of those levels is an error.
pub fn resolve_ui_mode(
    file: &super::File,
    env: &std::collections::HashMap<String, String>,
    override_value: &str,
) -> Result<UiMode, ConfigError> {
    let otto_ui = env.get("OTTO_UI").map(String::as_str).unwrap_or("");
    for candidate in [override_value, otto_ui, file.ui.mode.as_str()] {
        if let Some(mode) = parse_ui_mode(candidate)? {
            if let Some(forced) = file
                .policy
                .as_ref()
                .and_then(|p| p.force.get("ui"))
                .and_then(toml::Value::as_table)
                .and_then(|p| p.get("mode"))
                .and_then(toml::Value::as_str)
                && UiMode::parse(forced) != Some(mode)
            {
                return Err(ConfigError::new("UI mode is forced by managed policy"));
            }
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

    #[test]
    fn observer_defaults_and_explicit_empty_bindings_are_distinct() {
        let file: File = toml::from_str("").expect("default configuration");
        assert_eq!(file.ui.observer, ObserverBindings::default());
        let file: File = toml::from_str(
            "[ui.observer]\ntoggle = \"\"\nopen = \"Ctrl+Left\"\nback = \"Ctrl+Right\"\n",
        )
        .expect("observer configuration");
        assert_eq!(file.ui.observer.toggle, "");
        assert_eq!(file.ui.observer.open, "Ctrl+Left");
        assert_eq!(file.ui.observer.back, "Ctrl+Right");
        assert!(toml::from_str::<File>("[ui.observer]\ntoggl = \"F6\"\n").is_err());
    }

    #[test]
    fn observer_partial_managed_override_preserves_other_bindings() {
        use crate::config::layered::{Layer, Merged};
        let mut merged = Merged::default();
        merged
            .add(
                "[ui.observer]\ntoggle = \"Alt+o\"\nopen = \"Ctrl+Left\"\n",
                "user",
                Layer::User,
                true,
            )
            .expect("user bindings");
        merged
            .add(
                "[ui.observer]\nback = \"F9\"\n",
                "managed",
                Layer::Managed,
                true,
            )
            .expect("managed back binding");
        let file: File = merged.values.try_into().expect("merged bindings");
        assert_eq!(file.ui.observer.toggle, "Alt+o");
        assert_eq!(file.ui.observer.open, "Ctrl+Left");
        assert_eq!(file.ui.observer.back, "F9");
    }

    #[test]
    fn project_configuration_cannot_replace_client_observer_bindings() {
        use crate::config::layered::{Layer, Merged};
        let mut merged = Merged::default();
        merged
            .add(
                "[ui.observer]\ntoggle = \"Alt+o\"\n",
                "user",
                Layer::User,
                true,
            )
            .expect("user configuration");
        merged
            .add(
                "[ui.observer]\ntoggle = \"Ctrl+Left\"\n",
                "project",
                Layer::Local,
                true,
            )
            .expect("project configuration");
        let file: File = merged
            .values
            .clone()
            .try_into()
            .expect("merged configuration");
        assert_eq!(file.ui.observer.toggle, "Alt+o");
        assert!(merged.diagnostics.iter().any(|diagnostic| {
            diagnostic.key == "ui.observer" && diagnostic.source == "project"
        }));
    }

    #[test]
    fn defaults_to_auto() {
        let mode = resolve_ui_mode(&File::default(), &HashMap::new(), "").expect("resolve");
        assert_eq!(mode, UiMode::Auto);
    }

    #[test]
    fn override_wins_over_env_and_file() {
        let mut file = File::default();
        file.ui.mode = "tui".into();
        let mut env = HashMap::new();
        env.insert("OTTO_UI".to_string(), "repl".to_string());
        let mode = resolve_ui_mode(&file, &env, "auto").expect("resolve");
        assert_eq!(mode, UiMode::Auto);
    }

    #[test]
    fn env_wins_over_file() {
        let mut file = File::default();
        file.ui.mode = "tui".into();
        let mut env = HashMap::new();
        env.insert("OTTO_UI".to_string(), "repl".to_string());
        let mode = resolve_ui_mode(&file, &env, "").expect("resolve");
        assert_eq!(mode, UiMode::Repl);
    }

    #[test]
    fn file_is_used_last() {
        let mut file = File::default();
        file.ui.mode = "tui".into();
        let mode = resolve_ui_mode(&file, &HashMap::new(), "").expect("resolve");
        assert_eq!(mode, UiMode::Tui);
    }

    #[test]
    fn rejects_unknown_mode() {
        let err = resolve_ui_mode(&File::default(), &HashMap::new(), "bogus").unwrap_err();
        assert!(err.to_string().contains("invalid ui mode"), "{err}");
    }
}
