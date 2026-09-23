//! Configuration schema, parsing, and resolution.
//!
//! This module is pure: it has no filesystem or environment access. [`parse`]
//! turns TOML text into a [`File`], and [`set_default_profile`] rewrites the
//! `default_profile` line in already-read text. The native crate's
//! `kite::config` reads and writes the files, reads the environment, and calls
//! back into this module for the schema and the resolution logic in
//! [`resolve`].
//!
//! Ownership: every type here is owned data; nothing borrows from the input
//! text after parsing.
//!
//! Errors: every [`ConfigError`] message is the text the CLI prints, and the
//! tests assert on that wording.

pub mod agents;
pub mod duration;
pub mod inbound;
pub mod mcp;
pub mod memory;
pub mod model_limits;
mod paths;
pub mod resolve;
pub mod sandbox;
pub mod sandbox_setup;
pub mod server;
pub mod skills;
pub mod ui;

use std::collections::HashMap;

use regex::Regex;
use serde::{Deserialize, Serialize};

pub use agents::{Agents, AgentsRuntime, resolve_agents};
pub use inbound::{FeishuRuntime, Inbound, resolve_feishu};
pub use mcp::{Mcp, McpAuth, McpRuntime, McpServer, McpServerRuntime, McpTransport, resolve_mcp};
pub use memory::{Memory, MemoryRuntime, MemorySQLite, resolve_memory};
pub use model_limits::ModelLimits;
pub use resolve::{CompactionRuntime, Overrides, Runtime, SessionDefaults, resolve};
pub use sandbox::{SandboxDriverMode, SandboxNetworkMode, SandboxSettings, resolve_sandbox};
pub use sandbox_setup::update_sandbox;
pub use server::{Server, ServerRuntime, resolve_server};
pub use skills::{Skills, SkillsRuntime, resolve_skills};
pub use ui::{UiMode, resolve_ui_mode};

/// Provider identifier for a base-URL-and-key OpenAI-compatible backend.
pub const PROVIDER_OPENAI_COMPATIBLE: &str = "openai-compatible";
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
    #[serde(default)]
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
    pub agents: Agents,
    #[serde(default, skip_serializing_if = "Inbound::is_default")]
    pub inbound: Inbound,
    #[serde(default)]
    pub server: Server,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
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
    #[serde(default)]
    pub allow_env: Vec<String>,
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

/// One `[profiles.<name>]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub api_key_env: String,
    pub context_window: Option<i64>,
    pub compaction_window: Option<i64>,
}

/// Parses `text` as a `config.toml` document, rejecting unknown fields at every
/// table.
pub fn parse(text: &str) -> Result<File, ConfigError> {
    match toml::from_str::<File>(text) {
        Ok(file) => Ok(file),
        Err(err) => {
            let message = err.message();
            if message.contains("unknown field") {
                Err(ConfigError::new(format!("unknown field: {err}")))
            } else {
                Err(ConfigError::new(err.to_string()))
            }
        }
    }
}

/// Serializes `file` back to TOML text, for `Save`. The text production is
/// pure; the native crate writes it to disk.
pub fn to_toml_string(file: &File) -> Result<String, ConfigError> {
    toml::to_string(file).map_err(|err| ConfigError::new(err.to_string()))
}

static DEFAULT_PROFILE_LINE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*default_profile\s*=\s*("(?:[^"\\]|\\.)*"|'[^']*')\s*(#.*)?$"#)
        .expect("static pattern compiles")
});

/// Rewrites the `default_profile` line of `content` to name `profile`,
/// preserving every other line and its comments. Inserts a new line at the top
/// when no `default_profile` line exists.
pub fn set_default_profile(content: &str, profile: &str) -> String {
    let line = format!("default_profile = {}", go_quote(profile));
    if DEFAULT_PROFILE_LINE.is_match(content) {
        return DEFAULT_PROFILE_LINE
            .replace_all(content, line.as_str())
            .into_owned();
    }
    if content.trim().is_empty() {
        return format!("{line}\n");
    }
    if content.ends_with('\n') {
        format!("{line}\n{content}")
    } else {
        format!("{line}\n{content}\n")
    }
}

/// Renders `value` as a double-quoted string literal for the identifier-like
/// profile names Kite accepts.
///
/// ponytail: escapes backslash, double quote, and ASCII control characters
/// (`\n`, `\t`, `\r`, `\xNN`) but passes non-ASCII text through unescaped
/// rather than reproducing `strconv.Quote`'s full Unicode-printability table.
/// Upgrade if a profile name with exotic Unicode ever needs it.
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
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn parses_a_minimal_profile() {
        let file = parse(
            r#"default_profile = "local"
[profiles.local]
provider = "openai-compatible"
model = "test-model"
thinking = "high"
base_url = "http://localhost:8080/v1"
api_key_env = "TEST_KEY"
"#,
        )
        .expect("parse");
        assert_eq!(file.default_profile, "local");
        let profile = &file.profiles["local"];
        assert_eq!(profile.provider, "openai-compatible");
        assert_eq!(profile.model, "test-model");
        assert_eq!(profile.thinking, "high");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_top_level_fields() {
        let err = parse(
            r#"default_profile = "local"
unknown = true
[profiles.local]
provider = "openai-compatible"
model = "test-model"
base_url = "http://localhost:8080/v1"
api_key_env = "TEST_KEY"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_preserves_absent_and_explicit_values() {
        let file = parse(
            r#"[agent.compaction]
auto = false
reserve_tokens = 0
keep_recent_tokens = -1

[profiles.local]
context_window = 0
compaction_window = -2
"#,
        )
        .expect("parse");
        assert_eq!(file.agent.compaction.auto, Some(false));
        assert_eq!(file.agent.compaction.reserve_tokens, Some(0));
        assert_eq!(file.agent.compaction.keep_recent_tokens, Some(-1));
        let profile = &file.profiles["local"];
        assert_eq!(profile.context_window, Some(0));
        assert_eq!(profile.compaction_window, Some(-2));

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
            r#"[profiles.bad]
provider = "openai-compatible"
model = "test-model"
base_url = "https://example.com/v1"
api_key = "secret"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn set_default_profile_updates_existing_line() {
        let content = r#"default_profile = "old"
[profiles.old]
provider = "openai-compatible"
model = "old-model"
base_url = "https://old.example/v1"
api_key_env = "OLD_KEY"
[profiles.new]
provider = "chatgpt"
model = "gpt-5-codex"
"#;
        let updated = set_default_profile(content, "new");
        assert!(updated.contains("default_profile = \"new\""));
        assert!(!updated.contains("default_profile = \"old\""));
        let file = parse(&updated).expect("parse");
        assert_eq!(file.default_profile, "new");
        assert_eq!(file.profiles["old"].model, "old-model");
        assert_eq!(file.profiles["new"].provider, "chatgpt");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn set_default_profile_inserts_missing_line() {
        let content = "[profiles.new]\nprovider = \"chatgpt\"\nmodel = \"gpt-5-codex\"\n";
        let updated = set_default_profile(content, "new");
        assert!(updated.starts_with("default_profile = \"new\"\n"));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn round_trips_through_to_toml_string() {
        let file = parse(
            r#"default_profile = "local"
[profiles.local]
provider = "openai-compatible"
model = "test-model"
base_url = "http://localhost:8080/v1"
api_key_env = "TEST_KEY"
"#,
        )
        .expect("parse");
        let text = to_toml_string(&file).expect("serialize");
        let reparsed = parse(&text).expect("reparse");
        assert_eq!(file, reparsed);
    }
}
