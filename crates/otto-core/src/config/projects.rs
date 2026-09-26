//! The `[projects]` table: per-directory trust records.
//!
//! `otto trust` (`crates/otto/src/cli/trust.rs`) is the only writer, appending
//! a `[projects."<canonical dir>"]` table as text rather than through this
//! schema, so existing comments survive. `otto serve`'s admission
//! (`crates/otto/src/cli/serve.rs`) is the only reader.

use serde::{Deserialize, Serialize};

/// One `[projects."<path>"]` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub trust_level: TrustLevel,
}

/// The only trust level. Any other value, or a missing `trust_level`, fails
/// [`super::parse`], so every command reports it when loading the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    Trusted,
}

#[cfg(test)]
mod tests {
    use crate::config::{File, parse};

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn no_projects_table_resolves_empty() {
        let file = File::default();
        assert!(file.projects.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn trusted_project_resolves_to_its_path() {
        let file = parse(
            r#"[projects."/Users/me/src/app"]
trust_level = "trusted"
"#,
        )
        .expect("parse");
        assert_eq!(
            file.projects.keys().collect::<Vec<_>>(),
            vec!["/Users/me/src/app"]
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn untrusted_level_is_a_config_error_naming_the_key() {
        let err = parse(
            r#"[projects."/Users/me/src/app"]
trust_level = "untrusted"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("trust_level"), "{err}");
        assert!(err.to_string().contains("untrusted"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn project_table_rejects_unknown_fields() {
        let err = parse(
            r#"[projects."/Users/me/src/app"]
trust_level = "trusted"
unknown = true
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }
}
