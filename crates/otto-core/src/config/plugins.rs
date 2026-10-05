//! Pure user-owned plugin preferences and environment credential bindings.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugins {
    pub enabled: Option<bool>,
    #[serde(default)]
    pub disabled: BTreeSet<String>,
    /// Logical `plugin:server` identities. Project layers cannot set bindings.
    #[serde(default)]
    pub bindings: BTreeMap<String, PluginBindings>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBindings {
    pub env: Option<BTreeMap<String, String>>,
    pub headers: Option<BTreeMap<String, String>>,
    pub auth: Option<String>,
    pub oauth_client_id: Option<String>,
    pub oauth_scopes: Option<Vec<String>>,
}

impl Plugins {
    /// Environment names referenced by user-owned credential templates.
    pub fn credential_variables(&self) -> BTreeSet<String> {
        self.bindings
            .values()
            .flat_map(|b| b.env.iter().chain(b.headers.iter()))
            .flat_map(|m| m.values())
            .flat_map(|value| value.split("${").skip(1))
            .filter_map(|reference| reference.split_once('}'))
            .map(|(inner, _)| inner.split(":-").next().unwrap_or_default().to_owned())
            .filter(|name| !name.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        File,
        layered::{Layer, Merged},
        parse,
    };

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn project_plugins_can_only_add_disabling_preferences() {
        let mut merged = Merged::default();
        merged.add("[plugins]\nenabled=false\ndisabled=['one']\n[plugins.bindings.'one:tools'.env]\nTOKEN='${TOKEN}'", "user", Layer::User, true).unwrap();
        merged.add("[plugins]\nenabled=true\ndisabled=['two']\n[plugins.bindings.'one:tools'.env]\nTOKEN='${EVIL}'", "project", Layer::Local, true).unwrap();
        let file = merged.file().unwrap();
        assert_eq!(file.plugins.enabled, Some(false));
        assert_eq!(
            file.plugins.disabled,
            BTreeSet::from(["one".into(), "two".into()])
        );
        assert_eq!(
            file.plugins.bindings["one:tools"].env.as_ref().unwrap()["TOKEN"],
            "${TOKEN}"
        );
        assert!(!format!("{:?}", merged.explained().values).contains("${TOKEN}"));
        assert!(
            merged
                .diagnostics
                .iter()
                .any(|d| d.key == "plugins.enabled")
        );
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn project_can_disable_but_not_change_binding_authority() {
        let mut merged = Merged::default();
        merged
            .add("[plugins]\nenabled=false", "project", Layer::Local, false)
            .unwrap();
        assert_eq!(merged.file().unwrap().plugins.enabled, Some(false));
        assert!(parse("[plugins]\nunknown=true").is_err());
        assert_ne!(File::default().plugins.enabled, Some(false));
    }
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn credential_names_are_explicit_and_deduplicated() {
        let file=parse("[plugins.bindings.'example:tools'.env]\nA='${TOKEN}'\nB='${TOKEN:-fallback}'\nC='literal'\n[plugins.bindings.'example:remote'.headers]\nX='${REMOTE_KEY}'").unwrap();
        assert_eq!(
            file.plugins.credential_variables(),
            BTreeSet::from(["TOKEN".into(), "REMOTE_KEY".into()])
        );
    }
}
