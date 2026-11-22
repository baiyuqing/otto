//! Precedence resolution: combines a parsed [`File`], an injected
//! environment map, session defaults, and CLI overrides into a [`Runtime`].
//!
//! `env` is read here by key (never via `std::env::var`, which core cannot
//! call): the native `otto::config` layer populates the map from the real
//! process environment before calling in.

use std::collections::HashMap;
use std::time::Duration;

use super::{
    CompactionConfig, ConfigError, File, ModelSelector, duration::parse_go_duration,
    model_limits::resolve_model_limits,
};

use budget::resolve_budget;
use compaction::{resolve_compaction, validate_thinking};
use deadlines::{resolve_progress, resolve_resilience};
use model_selection::{known_provider, select_model};

mod budget;
mod compaction;
mod deadlines;
mod model_selection;
mod server_bootstrap;

pub use server_bootstrap::resolve_server_bootstrap;

const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_CANCELLATION_GRACE: Duration = Duration::from_secs(5);
const DEFAULT_UNATTENDED_TURN_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_MAX_OUTPUT_BYTES: i64 = 51200;
const DEFAULT_COMPACTION_RESERVE: i64 = 16_384;
const DEFAULT_COMPACTION_KEEP: i64 = 20_000;
/// The built-in progress limits. They are repeated from the agent's own
/// defaults so the configuration layer stays free of agent types; a test
/// pins the two together.
const DEFAULT_PROGRESS_REPEATED_CALLS: u32 = 8;
const DEFAULT_PROGRESS_CONSECUTIVE_FAILURES: u32 = 12;
/// The largest accepted progress limit, so a mistyped value fails loudly
/// instead of acting as "off".
const MAX_PROGRESS_LIMIT: u32 = 1000;
const MINIMUM_COMPACTION_WINDOW: i64 = 4_096;
const MINIMUM_COMPACTION_TARGET: i64 = 1_024;

/// The model an already-resumed session used. Applied only when no model is
/// explicitly selected (an explicit selection always wins).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDefaults {
    /// API type recorded by a session that stored no selector.
    pub provider: String,
    /// Provider model ID recorded by the session.
    pub model: String,
    /// The stored `profile` field when it holds a `<provider>/<model_id>` selector.
    pub model_selector: String,
}

/// Explicit CLI overrides. An empty string or zero `Duration`/`0` means "not set".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `--model`: a `<provider>/<model_id>` selector, or a bare model ID
    /// when `provider` (deprecated `--provider`) pairs it.
    pub model: String,
    pub provider: String,
    pub base_url: String,
    pub thinking: String,
    pub shell_timeout: Duration,
    pub max_output_bytes: i64,
}

/// The resolved compaction targets and catalog windows for one model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionRuntime {
    pub auto: bool,
    pub context_window: i64,
    pub hard_input_window: i64,
    pub working_window: i64,
    pub max_output_tokens: i64,
    pub reserve_tokens: i64,
    pub keep_recent_tokens: i64,
}

/// Resolved cooperative deadlines for agent-owned operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineRuntime {
    pub turn_timeout: Option<Duration>,
    /// Step deadline for chat and `otto --prompt` turns; `None` disables it.
    pub unattended_turn_timeout: Option<Duration>,
    pub provider_timeout: Option<Duration>,
    pub cancellation_grace: Duration,
    pub subagent_timeout: Option<Duration>,
    pub workflow_step_timeout: Option<Duration>,
}

impl Default for DeadlineRuntime {
    fn default() -> Self {
        Self {
            turn_timeout: None,
            unattended_turn_timeout: Some(DEFAULT_UNATTENDED_TURN_TIMEOUT),
            provider_timeout: None,
            cancellation_grace: DEFAULT_CANCELLATION_GRACE,
            subagent_timeout: None,
            workflow_step_timeout: None,
        }
    }
}

/// Resolved deadline configuration for one process run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResilienceRuntime {
    pub deadlines: DeadlineRuntime,
}

/// The resolved limits that stop a turn making no progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressRuntime {
    pub repeated_calls: u32,
    pub consecutive_failures: u32,
}

impl Default for ProgressRuntime {
    fn default() -> Self {
        Self {
            repeated_calls: DEFAULT_PROGRESS_REPEATED_CALLS,
            consecutive_failures: DEFAULT_PROGRESS_CONSECUTIVE_FAILURES,
        }
    }
}

/// The fully resolved configuration for one process run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runtime {
    pub model_selector: ModelSelector,
    /// The provider name from the selector.
    pub provider_name: String,
    /// The API type of the provider: `claude`, `openai-compatible` or `chatgpt`.
    pub provider_type: String,
    pub base_url: String,
    /// Authorized endpoint; absent for fixed-backend ChatGPT.
    pub endpoint: Option<super::endpoint::TrustedEndpoint>,
    /// The model ID the provider API receives.
    pub model: String,
    /// Models configured for child tasks on this exact provider connection.
    pub subagent_models: Vec<String>,
    pub thinking: String,
    /// The actual secret value, resolved from `env` by `api_key_env` (or the
    /// `OTTO_API_KEY` fallback). Empty for the `chatgpt` provider.
    pub api_key: String,
    pub api_key_env: String,
    pub api_version: String,
    pub workspace_id_env: String,
    pub workspace_id: String,
    pub shell_timeout: Duration,
    pub max_output_bytes: i64,
    pub compaction: CompactionRuntime,
    pub resilience: ResilienceRuntime,
    pub progress: ProgressRuntime,
    pub budget: crate::agent::budget::BudgetPolicy,
}

fn env_value(env: &HashMap<String, String>, key: &str) -> String {
    env.get(key).cloned().unwrap_or_default()
}

/// Resolves `file`, `env`, `session`, and `overrides` into a [`Runtime`].
///
/// Precedence for the model: `--model`, then `OTTO_MODEL`, then the
/// resumed `session`'s model, then `default_model`. The provider's connection fields come
/// from its table; `overrides.base_url` and `overrides.thinking` replace the
/// provider's endpoint and the model's thinking effort.
pub fn resolve(
    file: &File,
    env: &HashMap<String, String>,
    session: &SessionDefaults,
    overrides: &Overrides,
) -> Result<Runtime, ConfigError> {
    resolve_impl(file, env, session, overrides, true)
}

/// Inspect effective model/runtime defaults without reading credential values.
pub fn resolve_without_credentials(
    file: &File,
    env: &HashMap<String, String>,
    session: &SessionDefaults,
    overrides: &Overrides,
) -> Result<Runtime, ConfigError> {
    resolve_impl(file, env, session, overrides, false)
}

fn resolve_impl(
    file: &File,
    env: &HashMap<String, String>,
    session: &SessionDefaults,
    overrides: &Overrides,
    credentials: bool,
) -> Result<Runtime, ConfigError> {
    file.approval.validate()?;
    let selector = select_model(file, env, session, overrides)?;
    known_provider(file, &selector)?;
    let provider_entry = file
        .provider_entry(&selector.provider)
        .expect("known_provider checked the name");
    let configured = provider_entry.connection(&selector.provider);
    let model_entry = provider_entry
        .models
        .get(&selector.model_id)
        .cloned()
        .unwrap_or_default();

    let provider = configured.provider_type.clone();
    let model = selector.model_id.clone();
    let mut thinking = model_entry.thinking.clone();
    let mut base_url = configured.base_url.clone();
    let mut api_key_env = configured.api_key_env;
    let api_version = configured.api_version.clone();
    let workspace_id_env = configured.workspace_id_env.clone();
    if !overrides.thinking.is_empty() {
        thinking = overrides.thinking.clone();
    }
    if !overrides.base_url.is_empty() {
        base_url = overrides.base_url.clone();
    }

    if !matches!(
        provider.as_str(),
        super::PROVIDER_OPENAI_COMPATIBLE | super::PROVIDER_CHATGPT | super::PROVIDER_CLAUDE
    ) {
        return Err(ConfigError::new(format!(
            "unsupported provider type \"{provider}\" for provider \"{}\"",
            selector.provider
        )));
    }
    validate_thinking(&thinking)?;

    let endpoint = if matches!(
        provider.as_str(),
        super::PROVIDER_OPENAI_COMPATIBLE | super::PROVIDER_CLAUDE
    ) {
        if provider == super::PROVIDER_CLAUDE {
            let bytes = api_version.as_bytes();
            if bytes.len() != 10
                || !bytes.iter().enumerate().all(|(index, byte)| {
                    if index == 4 || index == 7 {
                        *byte == b'-'
                    } else {
                        byte.is_ascii_digit()
                    }
                })
            {
                return Err(ConfigError::new(
                    "invalid Claude api_version: expected YYYY-MM-DD",
                ));
            }
        }
        if base_url.is_empty() {
            return Err(ConfigError::new("missing base_url"));
        }
        base_url = normalize_base_url(&base_url)
            .map_err(|err| ConfigError::new(format!("invalid base_url: {err}")))?;
        Some(
            super::endpoint::TrustedEndpoint::authorize_for_provider(
                &base_url,
                &selector.provider,
                &provider,
                &provider_entry.trusted_origins,
                &provider_entry.allow_http_origins,
            )
            .map_err(ConfigError::new)?,
        )
    } else {
        // The chatgpt provider uses a fixed backend URL and OAuth credentials
        // from the credential file, not a configured base_url or API key.
        base_url = String::new();
        None
    };
    if selector.provider == super::PROVIDER_DEEPSEEK {
        // An endpoint override must not send the dedicated default credential
        // to another origin. An explicit binding remains user-controlled.
        api_key_env = provider_entry.api_key_env;
        if api_key_env.is_empty()
            && endpoint.as_ref().is_some_and(|endpoint| {
                endpoint.route() == super::endpoint::EndpointRoute::OfficialDeepSeek
            })
        {
            api_key_env = super::providers::DEEPSEEK_DEFAULT_API_KEY_ENV.into();
        }
    }

    let mut shell_timeout = DEFAULT_SHELL_TIMEOUT;
    if !file.agent.shell_timeout.is_empty() {
        let nanos = parse_go_duration(&file.agent.shell_timeout)
            .map_err(|err| ConfigError::new(format!("invalid shell_timeout: {err}")))?;
        if nanos <= 0 {
            return Err(ConfigError::new(
                "invalid shell_timeout: must be greater than zero",
            ));
        }
        shell_timeout = Duration::from_nanos(nanos as u64);
    }
    if overrides.shell_timeout > Duration::ZERO {
        shell_timeout = overrides.shell_timeout;
    }

    let mut max_output_bytes = DEFAULT_MAX_OUTPUT_BYTES;
    if file.agent.max_output_bytes > 0 {
        max_output_bytes = file.agent.max_output_bytes;
    }
    if overrides.max_output_bytes > 0 {
        max_output_bytes = overrides.max_output_bytes;
    }

    let compaction = resolve_compaction(&file.agent.compaction, &model_entry, &model)?;
    let resilience = resolve_resilience(&file.agent)?;
    let progress = resolve_progress(&file.agent)?;
    let budget = resolve_budget(&file.agent.budget)?;

    let mut api_key = String::new();
    if matches!(
        provider.as_str(),
        super::PROVIDER_OPENAI_COMPATIBLE | super::PROVIDER_CLAUDE
    ) && credentials
    {
        api_key = resolve_api_key(env, &api_key_env);
        // An unbound key is not a credential binding: the process starts and
        // provider requests fail until the user sets one or switches models.
        if let Some(allowed) = file
            .policy
            .as_ref()
            .and_then(|p| p.allowed_credential_variables.as_ref())
            .filter(|_| !api_key.is_empty())
        {
            let actual = if !api_key_env.is_empty() && !env_value(env, &api_key_env).is_empty() {
                api_key_env.as_str()
            } else {
                "OTTO_API_KEY"
            };
            if !allowed.iter().any(|name| name == actual) {
                return Err(ConfigError::new(
                    "provider credential binding is prohibited by managed policy",
                ));
            }
        }
    }

    let workspace_id =
        if provider == super::PROVIDER_CLAUDE && credentials && !workspace_id_env.is_empty() {
            if let Some(allowed) = file
                .policy
                .as_ref()
                .and_then(|policy| policy.allowed_credential_variables.as_ref())
                && !allowed.contains(&workspace_id_env)
            {
                return Err(ConfigError::new(
                    "provider workspace credential binding is prohibited by managed policy",
                ));
            }
            env_value(env, &workspace_id_env)
        } else {
            String::new()
        };
    let runtime = Runtime {
        provider_name: selector.provider.clone(),
        endpoint,
        model_selector: selector,
        provider_type: provider,
        base_url,
        model,
        subagent_models: Vec::new(),
        thinking,
        api_key,
        api_key_env,
        api_version,
        workspace_id_env,
        workspace_id,
        shell_timeout,
        max_output_bytes,
        compaction,
        resilience,
        progress,
        budget,
    };
    if let Some(policy) = &file.policy {
        policy.runtime(&runtime)?;
    }
    Ok(runtime)
}

/// The API key bound by `api_key_env`, then the fallback variable; empty when
/// neither is set. A missing key never blocks startup or model selection: the
/// provider client rejects requests until a key is bound.
fn resolve_api_key(env: &HashMap<String, String>, api_key_env: &str) -> String {
    if !api_key_env.is_empty() {
        let value = env_value(env, api_key_env);
        if !value.is_empty() {
            return value;
        }
        let fallback = env_value(env, super::providers::FALLBACK_API_KEY_ENV);
        if !fallback.is_empty() {
            return fallback;
        }
        return String::new();
    }
    let fallback = env_value(env, super::providers::FALLBACK_API_KEY_ENV);
    if !fallback.is_empty() {
        return fallback;
    }
    String::new()
}

/// Re-implements `otto::provider::openaicompat::normalize_base_url` on the
/// `url` crate.
///
/// ponytail: this duplicates (rather than calls)
/// `otto::provider::openaicompat::normalize_base_url`, because otto-core cannot
/// depend on the native `otto` crate that owns it. The tests below pin every
/// accepted and rejected form, so the two implementations stay in sync.
pub(super) fn normalize_base_url(base_url: &str) -> Result<String, &'static str> {
    super::endpoint::normalize_base_url(base_url)
}

#[cfg(test)]
mod tests;
