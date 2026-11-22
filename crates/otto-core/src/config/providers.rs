//! `[providers.<name>]` and `[providers.<name>.models."<model_id>"]` tables.
//!
//! A provider is one endpoint and credential binding of one API type.
//! Built-in bindings exist without a configured table; their names need not
//! match their wire types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::model_selector::validate_provider_name;
use super::{
    ConfigError, DEFAULT_CLAUDE_API_VERSION, File, PROVIDER_CHATGPT, PROVIDER_CLAUDE,
    PROVIDER_DEEPSEEK, PROVIDER_OPENAI_COMPATIBLE,
};

/// The supported provider wire types.
pub const PROVIDER_TYPES: [&str; 3] = [
    PROVIDER_CLAUDE,
    PROVIDER_OPENAI_COMPATIBLE,
    PROVIDER_CHATGPT,
];

/// The names of built-in provider bindings.
pub const BUILTIN_PROVIDERS: [&str; 4] = [
    PROVIDER_CLAUDE,
    PROVIDER_OPENAI_COMPATIBLE,
    PROVIDER_CHATGPT,
    PROVIDER_DEEPSEEK,
];
pub const DEEPSEEK_DEFAULT_BASE_URL: &str = "https://api.deepseek.com";
pub const DEEPSEEK_DEFAULT_API_KEY_ENV: &str = concat!("DEEPSEEK", "_API_KEY");
pub const CLAUDE_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// The API key variable used when a provider names none or its named variable is unset.
pub const FALLBACK_API_KEY_ENV: &str = "OTTO_API_KEY";
pub const CLAUDE_DEFAULT_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

pub fn is_builtin_provider(name: &str) -> bool {
    BUILTIN_PROVIDERS.contains(&name)
}

pub(super) fn builtin_provider_type(name: &str) -> Option<&str> {
    is_builtin_provider(name).then_some(match name {
        PROVIDER_DEEPSEEK => PROVIDER_OPENAI_COMPATIBLE,
        _ => name,
    })
}

fn default_include_preset_models() -> bool {
    true
}
fn include_preset_models_is_default(value: &bool) -> bool {
    *value
}

/// One `[providers.<name>]` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_origins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_http_origins: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key_env: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub workspace_id_env: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelEntry>,
    /// Include static picker entries alongside explicit model tables.
    #[serde(
        default = "default_include_preset_models",
        skip_serializing_if = "include_preset_models_is_default"
    )]
    pub include_preset_models: bool,
}

impl Default for ProviderEntry {
    fn default() -> Self {
        Self {
            type_: String::new(),
            base_url: String::new(),
            trusted_origins: Vec::new(),
            allow_http_origins: Vec::new(),
            api_key_env: String::new(),
            api_version: String::new(),
            workspace_id_env: String::new(),
            models: BTreeMap::new(),
            include_preset_models: true,
        }
    }
}

/// One `[providers.<name>.models."<model_id>"]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    pub context_window: Option<i64>,
    pub compaction_window: Option<i64>,
}

/// The connection fields of a provider after the API type's defaults apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderConnection {
    pub provider_type: String,
    pub base_url: String,
    pub api_key_env: String,
    pub api_version: String,
    pub workspace_id_env: String,
}

impl ProviderEntry {
    /// The API type of the provider named `name`; empty when a custom
    /// provider has no `type`.
    pub fn provider_type<'a>(&'a self, name: &'a str) -> &'a str {
        if !self.type_.is_empty() {
            &self.type_
        } else {
            builtin_provider_type(name).unwrap_or_default()
        }
    }

    /// The effective connection fields. DeepSeek supplies its official URL and
    /// only defaults its dedicated key variable for that origin. Claude gets
    /// `https://api.anthropic.com`, `ANTHROPIC_API_KEY` and the default API
    /// version. Other bindings keep their configured values.
    pub fn connection(&self, name: &str) -> ProviderConnection {
        let provider_type = self.provider_type(name).to_string();
        let mut connection = ProviderConnection {
            provider_type,
            base_url: self.base_url.clone(),
            api_key_env: self.api_key_env.clone(),
            api_version: self.api_version.clone(),
            workspace_id_env: self.workspace_id_env.clone(),
        };
        if connection.provider_type == PROVIDER_CLAUDE {
            default_if_empty(&mut connection.base_url, CLAUDE_DEFAULT_BASE_URL);
            default_if_empty(&mut connection.api_key_env, CLAUDE_DEFAULT_API_KEY_ENV);
            default_if_empty(&mut connection.api_version, DEFAULT_CLAUDE_API_VERSION);
        }
        if name == PROVIDER_DEEPSEEK {
            default_if_empty(&mut connection.base_url, DEEPSEEK_DEFAULT_BASE_URL);
            if super::endpoint::is_deepseek_origin(&connection.base_url) {
                default_if_empty(&mut connection.api_key_env, DEEPSEEK_DEFAULT_API_KEY_ENV);
            }
        }
        connection
    }

    fn validate(&self, name: &str) -> Result<(), ConfigError> {
        let table = format!("providers.{name}");
        validate_provider_name(name)
            .map_err(|reason| ConfigError::new(format!("{table}: {reason}")))?;
        if let Some(expected) = builtin_provider_type(name) {
            if !self.type_.is_empty() && self.type_ != expected {
                return Err(ConfigError::new(format!(
                    "{table}.type must be \"{expected}\" or omitted"
                )));
            }
        } else if self.type_.is_empty() {
            return Err(ConfigError::new(format!(
                "{table}.type is required: \"claude\", \"openai-compatible\" or \"chatgpt\""
            )));
        } else if !PROVIDER_TYPES.contains(&self.type_.as_str()) {
            return Err(ConfigError::new(format!(
                "{table}.type must be \"claude\", \"openai-compatible\" or \"chatgpt\", got \"{}\"",
                self.type_
            )));
        }
        if !self.base_url.is_empty() {
            super::endpoint::normalize_base_url(&self.base_url).map_err(ConfigError::new)?;
        }
        for origin in self.trusted_origins.iter().chain(&self.allow_http_origins) {
            super::endpoint::validate_origin_grant(origin).map_err(ConfigError::new)?;
        }
        let provider_type = self.provider_type(name);
        let rejected: &[(&str, &String)] = match provider_type {
            PROVIDER_CHATGPT => &[
                ("base_url", &self.base_url),
                ("api_key_env", &self.api_key_env),
                ("api_version", &self.api_version),
                ("workspace_id_env", &self.workspace_id_env),
            ],
            PROVIDER_OPENAI_COMPATIBLE => &[
                ("api_version", &self.api_version),
                ("workspace_id_env", &self.workspace_id_env),
            ],
            _ => &[],
        };
        if let Some((field, _)) = rejected.iter().find(|(_, value)| !value.is_empty()) {
            return Err(ConfigError::new(format!(
                "{table}.{field} is not allowed for type \"{provider_type}\""
            )));
        }
        if provider_type == PROVIDER_OPENAI_COMPATIBLE
            && !is_builtin_provider(name)
            && self.base_url.is_empty()
        {
            return Err(ConfigError::new(format!(
                "{table}.base_url is required for type \"openai-compatible\""
            )));
        }
        if self.models.contains_key("") {
            return Err(ConfigError::new(format!(
                "{table}.models: model ID must not be empty"
            )));
        }
        Ok(())
    }
}

fn default_if_empty(value: &mut String, default: &str) {
    if value.is_empty() {
        *value = default.into();
    }
}

impl File {
    /// Validates every `[providers.<name>]` table, naming the table and field
    /// of the first violation.
    pub fn validate_providers(&self) -> Result<(), ConfigError> {
        for (name, entry) in &self.providers {
            entry.validate(name)?;
        }
        Ok(())
    }

    /// The effective provider, with static model tables filled in when enabled.
    /// Explicit model tables always win; the raw configuration is unchanged.
    pub fn provider_entry(&self, name: &str) -> Option<ProviderEntry> {
        let mut entry = self
            .providers
            .get(name)
            .cloned()
            .or_else(|| is_builtin_provider(name).then(ProviderEntry::default))?;
        if entry.include_preset_models {
            for &model_id in super::model_presets::for_provider(name) {
                entry.models.entry(model_id.into()).or_default();
            }
        }
        Some(entry)
    }

    /// The model table `selector` names, or an empty one when the provider has
    /// no table for that model ID. `None` when the provider is unknown.
    pub fn model_entry(&self, selector: &super::ModelSelector) -> Option<ModelEntry> {
        let provider = self.provider_entry(&selector.provider)?;
        Some(
            provider
                .models
                .get(&selector.model_id)
                .cloned()
                .unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::config::parse;

    fn parse_error(text: &str) -> String {
        parse(text).expect_err("must be rejected").to_string()
    }

    #[test]
    fn builtin_providers_need_no_table_and_accept_models_and_overrides() {
        let file = parse(
            r#"default_model = "chatgpt/gpt-5.6-luna"
[providers.chatgpt.models."gpt-5.6-luna"]
[providers.chatgpt.models."gpt-5.6-sol"]
thinking = "high"
[providers.claude]
type = "claude"
api_key_env = "MY_ANTHROPIC_KEY"
[providers.claude.models."claude-x"]
context_window = 200000
compaction_window = 100000
"#,
        )
        .unwrap();
        assert_eq!(file.default_model, "chatgpt/gpt-5.6-luna");
        let chatgpt = &file.providers["chatgpt"];
        assert_eq!(chatgpt.type_, "");
        assert_eq!(chatgpt.models["gpt-5.6-sol"].thinking, "high");
        assert!(chatgpt.models["gpt-5.6-luna"].context_window.is_none());
        let claude = file.provider_entry("claude").unwrap();
        let connection = claude.connection("claude");
        assert_eq!(connection.api_key_env, "MY_ANTHROPIC_KEY");
        assert_eq!(connection.base_url, "https://api.anthropic.com");
        assert_eq!(claude.models["claude-x"].compaction_window, Some(100_000));
        assert!(file.provider_entry("openai-compatible").is_some());
        let deepseek = file.provider_entry("deepseek").expect("built-in DeepSeek");
        assert_eq!(deepseek.provider_type("deepseek"), "openai-compatible");
        assert!(deepseek.models.contains_key("deepseek-flash"));
        assert!(file.provider_entry("unknown-provider").is_none());
    }

    #[test]
    fn custom_provider_with_models_parses() {
        let file = parse(
            r#"[providers.deepseek]
type = "openai-compatible"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
[providers.deepseek.models."deepseek-chat"]
thinking = "high"
"#,
        )
        .unwrap();
        let provider = &file.providers["deepseek"];
        assert_eq!(provider.provider_type("deepseek"), "openai-compatible");
        assert_eq!(provider.models["deepseek-chat"].thinking, "high");
    }

    #[test]
    fn validation_errors_name_the_table_and_field() {
        let cases = [
            (
                "[providers.claude]\ntype = \"chatgpt\"\n",
                "providers.claude.type must be \"claude\" or omitted",
            ),
            (
                "[providers.x]\ntype = \"ollama\"\n",
                "providers.x.type must be \"claude\", \"openai-compatible\" or \"chatgpt\"",
            ),
            (
                "[providers.x]\nbase_url = \"https://a/v1\"\n",
                "providers.x.type is required",
            ),
            (
                "[providers.chatgpt]\nbase_url = \"https://a/v1\"\n",
                "providers.chatgpt.base_url is not allowed for type \"chatgpt\"",
            ),
            (
                "[providers.chatgpt]\napi_key_env = \"K\"\n",
                "providers.chatgpt.api_key_env is not allowed",
            ),
            (
                "[providers.x]\ntype = \"openai-compatible\"\nbase_url = \"https://a/v1\"\napi_version = \"2026-10-21\"\n",
                "providers.x.api_version is not allowed for type \"openai-compatible\"",
            ),
            (
                "[providers.x]\ntype = \"openai-compatible\"\n",
                "providers.x.base_url is required",
            ),
            (
                "[providers.\"a/b\"]\ntype = \"chatgpt\"\n",
                "providers.a/b: provider name \"a/b\"",
            ),
            (
                "[providers.chatgpt.models.\"\"]\n",
                "providers.chatgpt.models: model ID must not be empty",
            ),
        ];
        for (text, expected) in cases {
            let error = parse_error(text);
            assert!(error.contains(expected), "{text}\n  got: {error}");
        }
    }

    #[test]
    fn unknown_provider_and_model_fields_are_rejected() {
        assert!(parse_error("[providers.chatgpt]\napi_key = \"secret\"\n").contains("unknown"));
        assert!(
            parse_error("[providers.chatgpt.models.\"m\"]\nbase_url = \"x\"\n").contains("unknown")
        );
    }
}
