//! The `[inbound]` table: host-side event sources that push the session inbox.

use serde::{Deserialize, Serialize};

const DEFAULT_BINARY: &str = "lark-cli";

/// The `[inbound]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inbound {
    #[serde(default)]
    pub feishu: Feishu,
}

impl Inbound {
    pub(super) fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// The `[inbound.feishu]` table. Secrets stay in `lark-cli`'s own store, not
/// here. `enabled` defaults to off, and stays off in effect until `chat_ids`
/// lists at least one chat.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feishu {
    pub enabled: Option<bool>,
    /// `lark-cli` by default. An empty string is treated as absent.
    pub binary: Option<String>,
    /// Allowlist of `chat_id` values that may deliver. Empty admits nothing,
    /// so inbound stays off until at least one chat is listed.
    #[serde(default)]
    pub chat_ids: Vec<String>,
}

/// Resolved Feishu inbound settings for one `kite serve` process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeishuRuntime {
    pub enabled: bool,
    pub binary: String,
    pub chat_ids: Vec<String>,
}

/// Resolves `[inbound.feishu]`. Never errors.
pub fn resolve_feishu(file: &super::File) -> FeishuRuntime {
    let configured = &file.inbound.feishu;
    FeishuRuntime {
        enabled: configured.enabled.unwrap_or(false),
        binary: configured
            .binary
            .as_deref()
            .and_then(nonempty_trimmed)
            .unwrap_or_else(|| DEFAULT_BINARY.to_string()),
        chat_ids: configured
            .chat_ids
            .iter()
            .filter_map(|id| nonempty_trimmed(id))
            .collect(),
    }
}

fn nonempty_trimmed(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use crate::config::{parse, resolve_feishu as resolve};

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn absent_inbound_is_disabled_with_the_default_binary() {
        let file = parse("").expect("parse");
        let runtime = resolve(&file);
        assert!(!runtime.enabled);
        assert_eq!(runtime.binary, super::DEFAULT_BINARY);
        assert!(runtime.chat_ids.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn enabled_feishu_keeps_chat_ids_and_a_custom_binary() {
        let file = parse(
            r#"
[inbound.feishu]
enabled = true
binary = " /opt/lark-cli "
chat_ids = [" oc_a ", "", "oc_b"]
"#,
        )
        .expect("parse");
        let runtime = resolve(&file);
        assert!(runtime.enabled);
        assert_eq!(runtime.binary, "/opt/lark-cli");
        assert_eq!(runtime.chat_ids, ["oc_a", "oc_b"]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn an_empty_binary_falls_back_to_lark_cli() {
        let file = parse(
            r#"
[inbound.feishu]
enabled = true
binary = "   "
"#,
        )
        .expect("parse");
        assert_eq!(resolve(&file).binary, super::DEFAULT_BINARY);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn a_token_field_is_rejected() {
        let err = parse(
            r#"
[inbound.feishu]
enabled = true
token = "secret"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }
}
