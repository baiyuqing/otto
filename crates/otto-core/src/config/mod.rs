//! Configuration schema, parsing, and resolution.
//!
//! This module is pure: it has no filesystem or environment access. [`parse`]
//! turns TOML text into a [`File`], and [`edit`] changes one key or table of
//! already-read text in place. The native crate's
//! `otto::config` reads and writes the files, reads the environment, and calls
//! back into this module for the schema and the resolution logic in
//! [`resolve`].
//!
//! Ownership: every type here is owned data; nothing borrows from the input
//! text after parsing.
//!
//! Errors: every [`ConfigError`] message is the text the CLI prints, and the
//! tests assert on that wording.

pub mod agents;
pub mod approval;
pub mod duration;
pub mod edit;
pub mod endpoint;
pub mod failover;
pub mod layered;
pub mod legacy_profiles;
pub mod mcp;
pub mod memory;
pub mod model_limits;
pub mod model_selector;
mod paths;
pub mod plugins;
pub mod projects;
pub mod providers;
pub mod resolve;
pub mod sandbox;
pub mod sandbox_setup;
pub mod server;
pub mod skills;
pub mod ui;

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

pub use agents::{Agents, AgentsRuntime, resolve_agents};
pub use approval::ApprovalConfig;
pub use failover::{Failover, FailoverRuntime, resolve_failover};
pub use legacy_profiles::{LegacyProfile, convert_legacy_profiles};
pub use mcp::{Mcp, McpAuth, McpRuntime, McpServer, McpServerRuntime, McpTransport, resolve_mcp};
pub use memory::{Memory, MemoryRuntime, MemoryTurso, resolve_memory};
pub use model_limits::ModelLimits;
pub use model_selector::ModelSelector;
pub use plugins::{PluginBindings, Plugins};
pub use projects::{Project, TrustLevel};
pub use providers::{ModelEntry, ProviderConnection, ProviderEntry};
pub use resolve::{
    CompactionRuntime, DeadlineRuntime, Overrides, ResilienceRuntime, Runtime, SessionDefaults,
    resolve,
};
pub use sandbox::{SandboxDriverMode, SandboxNetworkMode, SandboxSettings, resolve_sandbox};
pub use sandbox_setup::update_sandbox;
pub use server::{Server, ServerRuntime, ServerTls, resolve_server};
pub use skills::{Skills, SkillsRuntime, resolve_skills};
pub use ui::{UiMode, resolve_ui_mode};

/// Provider identifier for a base-URL-and-key OpenAI-compatible backend.
pub const PROVIDER_OPENAI_COMPATIBLE: &str = "openai-compatible";
pub const PROVIDER_CLAUDE: &str = "claude";
pub const DEFAULT_CLAUDE_API_VERSION: &str = "2026-10-21";
/// Provider identifier for a ChatGPT subscription via OAuth credentials.
pub const PROVIDER_CHATGPT: &str = "chatgpt";

/// A configuration error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(String);

impl ConfigError {
    /// Builds an error carrying `message` verbatim.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// The full contents of one `config.toml` file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    #[serde(default, skip_serializing_if = "ApprovalConfig::is_default")]
    pub approval: ApprovalConfig,
    /// Runtime-only authority metadata; never accepted from TOML or written back.
    #[serde(skip)]
    pub policy: Option<layered::ManagedPolicy>,
    #[serde(skip)]
    pub project_executables: Option<bool>,
    #[serde(skip)]
    pub executable_digest: Option<String>,
    #[serde(skip)]
    pub user_values: Option<toml::Table>,
    #[serde(skip)]
    pub user_source: Option<String>,
    #[serde(skip)]
    pub diagnostics: Vec<layered::Diagnostic>,
    /// The default model selector, `<provider>/<model_id>`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_model: String,
    /// Deprecated input; cleared by [`convert_legacy_profiles`] once it names a profile.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_profile: String,
    #[serde(default)]
    pub ui: ui::Ui,
    #[serde(default)]
    pub agent: Agent,
    #[serde(default)]
    pub memory: Memory,
    #[serde(default)]
    pub mcp: Mcp,
    #[serde(default)]
    pub skills: Skills,
    #[serde(default)]
    pub plugins: Plugins,
    #[serde(default, skip_serializing_if = "plugins::Hooks::is_default")]
    pub hooks: plugins::Hooks,
    #[serde(default)]
    pub agents: Agents,
    #[serde(default, skip_serializing_if = "Experimental::is_default")]
    pub experimental: Experimental,
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub failover: Failover,
    #[serde(default, skip_serializing_if = "Web::is_default")]
    pub web: Web,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderEntry>,
    /// Deprecated input; emptied by [`convert_legacy_profiles`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, LegacyProfile>,
    /// Selector of the model each deprecated profile was converted to.
    #[serde(skip)]
    pub legacy_profile_selectors: BTreeMap<String, ModelSelector>,
    #[serde(default)]
    pub projects: HashMap<String, projects::Project>,
}

/// Legacy `[experimental]` configuration, accepted for compatibility.
/// New configuration uses `[skills].typesafe_skill_check`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Experimental {
    pub typesafe_skill_check: Option<bool>,
    #[serde(flatten)]
    unknown: std::collections::BTreeMap<String, toml::Value>,
}

impl Experimental {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Legacy `[web]` configuration, accepted for compatibility.
/// Public web access no longer requires host grants; `allow_hosts` is ignored.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    #[serde(default)]
    pub allow_hosts: Vec<String>,
}

impl Web {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The `[sandbox]` table as written in the config file, before resolution.
///
/// `driver` and `network` are `Option<String>` because absent and
/// explicit-empty are different: [`resolve_sandbox`] rejects an explicit
/// empty string but falls back to a default when the key is absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    pub driver: Option<String>,
    pub network: Option<String>,
    #[serde(default)]
    pub read_paths: Vec<String>,
    /// Paths added to the built-in sandbox read denylist; relative to the
    /// home directory when written as `~/...`.
    #[serde(default)]
    pub deny_read_paths: Vec<String>,
    #[serde(default)]
    pub allow_env: Vec<String>,
    /// Commands that run outside the sandbox; see
    /// [`sandbox::excluded_command_matches`].
    #[serde(default)]
    pub excluded_commands: Vec<String>,
}

/// The `[agent]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    #[serde(default)]
    pub max_turns: i64,
    #[serde(default)]
    pub shell_timeout: String,
    #[serde(default)]
    pub max_output_bytes: i64,
    pub turn_timeout: Option<String>,
    /// Deadline for chat and `otto --prompt` turns. `None` keeps the default
    /// of 30 minutes; `0` disables it.
    pub unattended_turn_timeout: Option<String>,
    pub provider_timeout: Option<String>,
    pub cancellation_grace: Option<String>,
    pub subagent_timeout: Option<String>,
    pub workflow_step_timeout: Option<String>,
    /// Repetitions of identical consecutive calls or unchanged failed short
    /// cycles that stop a turn making no progress.
    /// `None` keeps the built-in default; `0` disables this condition.
    pub no_progress_repeats: Option<i64>,
    /// Consecutive failed tool results that stop a turn making no progress.
    /// `None` keeps the built-in default; `0` disables this condition.
    pub no_progress_failures: Option<i64>,
    #[serde(default)]
    pub compaction: CompactionConfig,
}

/// The `[agent.compaction]` table. Every field is optional so absent and
/// explicit-zero/negative stay distinguishable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionConfig {
    pub auto: Option<bool>,
    pub reserve_tokens: Option<i64>,
    pub keep_recent_tokens: Option<i64>,
}

/// Parses `text` as a `config.toml` document, rejecting unknown fields at every
/// table.
pub fn parse(text: &str) -> Result<File, ConfigError> {
    match toml::from_str::<File>(text) {
        Ok(mut file) => {
            let table: toml::Table =
                toml::from_str(text).map_err(|err| ConfigError::new(err.to_string()))?;
            legacy_profiles::reject_mixed_source(&table, "configuration")?;
            file.approval.validate()?;
            file.validate_providers()?;
            convert_legacy_profiles(&mut file)?;
            Ok(file)
        }
        Err(err) => {
            if let Some(removed) = removed_table_error(text, &err) {
                Err(ConfigError::new(removed))
            } else if err.message().contains("unknown field") {
                Err(ConfigError::new(format!("unknown field: {err}")))
            } else {
                Err(ConfigError::new(err.to_string()))
            }
        }
    }
}

/// The migration message when `err` rejects the `[memory.remote]` table that
/// earlier releases accepted. Only a table that the document really holds
/// matches, so an unrelated `remote` key keeps the generic error.
pub(crate) fn removed_table_error(text: &str, err: &toml::de::Error) -> Option<&'static str> {
    if !err.message().starts_with("unknown field `remote`") {
        return None;
    }
    let table = toml::from_str::<toml::Table>(text).ok()?;
    let memory_has_remote = table
        .get("memory")
        .and_then(toml::Value::as_table)
        .is_some_and(|memory| memory.contains_key("remote"));
    memory_has_remote.then_some(concat!(
        "[memory.remote] was removed: install the external memory service as an Otto plugin, ",
        "such as otto-mem (https://github.com/baiyuqing/otto-mem), set backend = \"plugin:otto-mem\" ",
        "in [memory], bind its token under [plugins.bindings.otto-mem], and delete [memory.remote] from config.toml"
    ))
}

/// Sets the top-level `default_model` of `content` to the selector `model`,
/// changing only that value; see [`edit::set_value`] for the layout and errors.
pub fn set_default_model(content: &str, model: &str) -> Result<String, ConfigError> {
    let selector = ModelSelector::parse(model)?;
    edit::set_value(
        content,
        &[],
        "default_model",
        Some(&go_quote(&selector.to_string())),
    )
}

/// Renders `value` as a double-quoted string literal for the identifier-like
/// names and selectors Otto accepts.
///
/// ponytail: escapes backslash, double quote, and ASCII control characters
/// (`\n`, `\t`, `\r`, `\xNN`) but passes non-ASCII text through unescaped
/// rather than reproducing `strconv.Quote`'s full Unicode-printability table.
/// Upgrade if a name with exotic Unicode ever needs it.
fn go_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod legacy_profiles_tests;

#[cfg(test)]
mod sample_config_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn unknown_tables_and_keys_keep_the_generic_error() {
        for text in [
            "bogus = 1\n",
            "[inbound]\nenabled = true\n",
            "[server]\ninbound = 1\n",
        ] {
            let message = parse(text).expect_err("unknown key").to_string();
            assert!(message.starts_with("unknown field:"), "{message}");
            assert!(!message.contains("was removed"), "{message}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn removed_memory_remote_table_gets_a_specific_error() {
        let text = parse("[memory.remote]\nurl = \"https://mem.example\"\n")
            .expect_err("memory.remote is removed")
            .to_string();
        assert!(text.starts_with("[memory.remote] was removed:"), "{text}");
        assert!(text.contains("[plugins.bindings.otto-mem]"), "{text}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn other_unknown_remote_keys_keep_the_generic_error() {
        for text in [
            "[server]\nremote = 1\n",
            "remote = 1\n",
            "[memory]\nbogus = 1\n",
        ] {
            let message = parse(text).expect_err("unknown key").to_string();
            assert!(message.starts_with("unknown field:"), "{message}");
            assert!(!message.contains("was removed"), "{message}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn parses_a_provider_with_a_model() {
        let file = parse(
            r#"default_model = "local/test-model"
[providers.local]
type = "openai-compatible"
base_url = "http://localhost:8080/v1"
api_key_env = "TEST_KEY"
[providers.local.models."test-model"]
thinking = "high"
"#,
        )
        .expect("parse");
        assert_eq!(file.default_model, "local/test-model");
        let provider = &file.providers["local"];
        assert_eq!(provider.type_, "openai-compatible");
        assert_eq!(provider.base_url, "http://localhost:8080/v1");
        assert_eq!(provider.models["test-model"].thinking, "high");
        assert!(file.profiles.is_empty());
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_top_level_fields() {
        let err = parse(
            r#"default_model = "chatgpt/x"
unknown = true
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn deadlines_preserve_absent_and_explicit_values() {
        let empty = parse("").expect("parse");
        assert_eq!(empty.agent.turn_timeout, None);
        assert_eq!(empty.agent.provider_timeout, None);
        assert_eq!(empty.agent.cancellation_grace, None);
        assert_eq!(empty.agent.subagent_timeout, None);
        assert_eq!(empty.agent.workflow_step_timeout, None);

        let file = parse(
            r#"[agent]
turn_timeout = ""
provider_timeout = "0s"
cancellation_grace = "-1s"
subagent_timeout = "2m"
workflow_step_timeout = "3h"
"#,
        )
        .expect("parse");
        assert_eq!(file.agent.turn_timeout.as_deref(), Some(""));
        assert_eq!(file.agent.provider_timeout.as_deref(), Some("0s"));
        assert_eq!(file.agent.cancellation_grace.as_deref(), Some("-1s"));
        assert_eq!(file.agent.subagent_timeout.as_deref(), Some("2m"));
        assert_eq!(file.agent.workflow_step_timeout.as_deref(), Some("3h"));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_removed_agent_retry_table() {
        let err = parse("[agent.retry]\nmax_attempts = 3\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
        assert!(err.to_string().contains("retry"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_preserves_absent_and_explicit_values() {
        let file = parse(
            r#"[agent.compaction]
auto = false
reserve_tokens = 0
keep_recent_tokens = -1

[providers.local]
type = "openai-compatible"
base_url = "http://localhost/v1"
[providers.local.models."m"]
context_window = 0
compaction_window = -2
"#,
        )
        .expect("parse");
        assert_eq!(file.agent.compaction.auto, Some(false));
        assert_eq!(file.agent.compaction.reserve_tokens, Some(0));
        assert_eq!(file.agent.compaction.keep_recent_tokens, Some(-1));
        let model = &file.providers["local"].models["m"];
        assert_eq!(model.context_window, Some(0));
        assert_eq!(model.compaction_window, Some(-2));

        let empty = parse("").expect("parse");
        assert_eq!(empty.agent.compaction.auto, None);
        assert_eq!(empty.agent.compaction.reserve_tokens, None);
        assert_eq!(empty.agent.compaction.keep_recent_tokens, None);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_rejects_unknown_fields() {
        let err = parse("[agent.compaction]\nunknown = true\n").unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn ui_decodes_mode() {
        let file = parse("[ui]\nmode = \"auto\"\n").expect("parse");
        assert_eq!(file.ui.mode, "auto");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn ui_rejects_unknown_fields() {
        let err = parse("[ui]\nmode = \"auto\"\nunknown = true\n").unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn sandbox_preserves_absent_and_explicit_values() {
        let file = parse("").expect("parse");
        assert_eq!(file.sandbox.driver, None);
        assert_eq!(file.sandbox.network, None);
        assert!(file.sandbox.read_paths.is_empty());
        assert!(file.sandbox.allow_env.is_empty());

        let file = parse(
            r#"[sandbox]
driver = ""
network = ""
read_paths = ["/opt/sdk", "~/source"]
allow_env = ["PATH", "PROJECT_TOKEN"]
"#,
        )
        .expect("parse");
        assert_eq!(file.sandbox.driver, Some(String::new()));
        assert_eq!(file.sandbox.network, Some(String::new()));
        assert_eq!(file.sandbox.read_paths, vec!["/opt/sdk", "~/source"]);
        assert_eq!(file.sandbox.allow_env, vec!["PATH", "PROJECT_TOKEN"]);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn sandbox_decodes_valid_table() {
        let file = parse(
            r#"[sandbox]
driver = "seatbelt"
network = "deny"
read_paths = ["/Library/Developer"]
allow_env = ["PROJECT_TOKEN"]
"#,
        )
        .expect("parse");
        assert_eq!(file.sandbox.driver.as_deref(), Some("seatbelt"));
        assert_eq!(file.sandbox.network.as_deref(), Some("deny"));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn sandbox_rejects_unknown_fields() {
        let err = parse("[sandbox]\ndriver = \"auto\"\nunknown = true\n").unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_raw_secret_field() {
        let err = parse(
            r#"[providers.bad]
type = "openai-compatible"
base_url = "https://example.com/v1"
api_key = "secret"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn set_default_model_updates_existing_line() {
        let content = r#"default_model = "chatgpt/old"
[providers.chatgpt.models."old"]
[providers.chatgpt.models."new"]
"#;
        let updated = set_default_model(content, "chatgpt/new").expect("set");
        assert!(updated.contains("default_model = \"chatgpt/new\""));
        assert!(!updated.contains("chatgpt/old\""));
        let file = parse(&updated).expect("parse");
        assert_eq!(file.default_model, "chatgpt/new");
        assert_eq!(file.providers["chatgpt"].models.len(), 2);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn set_default_model_inserts_missing_line_and_rejects_a_bare_id() {
        let content = "[providers.chatgpt.models.\"new\"]\n";
        let updated = set_default_model(content, "chatgpt/new").expect("set");
        assert!(updated.starts_with("default_model = \"chatgpt/new\"\n"));
        let error = set_default_model(content, "new").unwrap_err().to_string();
        assert!(error.contains("<provider>/<model_id>"), "{error}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn experimental_absent_is_empty() {
        let file = parse("").expect("parse");
        assert_eq!(file.experimental.typesafe_skill_check, None);
        assert_eq!(file.skills.typesafe_skill_check, None);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn typesafe_skill_check_is_a_skills_setting() {
        let file = parse("[skills]\ntypesafe_skill_check = false\n").expect("parse");
        assert_eq!(file.skills.typesafe_skill_check, Some(false));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn experimental_unknown_key_parses() {
        let file = parse(
            r#"[experimental]
typesafe_skill_check = true
future_thing = "x"
"#,
        )
        .expect("parse");
        assert_eq!(file.experimental.typesafe_skill_check, Some(true));
    }
}

#[cfg(test)]
mod endpoint_tests;
