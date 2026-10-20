//! Precedence resolution: combines a parsed [`File`](super::File), an injected
//! environment map, session defaults, and CLI overrides into a [`Runtime`].
//!
//! `env` is read here by key (never via `std::env::var`, which core cannot
//! call): the native `otto::config` layer populates the map from the real
//! process environment before calling in.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;

use url::Url;

use super::{
    CompactionConfig, ConfigError, File, Profile, duration::parse_go_duration,
    model_limits::resolve_model_limits,
};

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

/// The provider and model an already-resumed session used. Applied only when
/// no profile is explicitly selected (an explicit profile always wins).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionDefaults {
    pub provider: String,
    pub model: String,
}

/// Explicit CLI overrides, applied after profile, environment, and session
/// defaults. An empty string or zero `Duration`/`0` means "not set".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub profile: String,
    pub provider: String,
    pub base_url: String,
    pub model: String,
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
    pub profile: String,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// Models configured for child tasks on this exact provider connection.
    pub subagent_models: Vec<String>,
    pub thinking: String,
    /// The actual secret value, resolved from `env` by `api_key_env` (or the
    /// `OTTO_API_KEY` fallback). Empty for the `chatgpt` provider.
    pub api_key: String,
    pub api_key_env: String,
    pub shell_timeout: Duration,
    pub max_output_bytes: i64,
    pub compaction: CompactionRuntime,
    pub resilience: ResilienceRuntime,
    pub progress: ProgressRuntime,
}

/// Explains connection-scoped child model fallback without exposing configured values.
/// Call with the runtime resolved from `file`; an empty configured list is not a fallback.
pub fn subagent_model_diagnostic(
    file: &File,
    runtime: &Runtime,
) -> Option<super::layered::Diagnostic> {
    let profile = file.profiles.get(&runtime.profile)?;
    if profile.subagent_models.is_empty() || !runtime.subagent_models.is_empty() {
        return None;
    }
    let same_connection = runtime.provider == profile.provider
        && (runtime.provider == super::PROVIDER_CHATGPT
            || (runtime.api_key_env == profile.api_key_env
                && normalize_base_url(&profile.base_url).ok().as_deref()
                    == Some(runtime.base_url.as_str())));
    if same_connection {
        return None;
    }
    Some(super::layered::Diagnostic {
        key: "subagent_models".into(),
        source: "runtime".into(),
        reason: "Configured subagent_models are disabled because the provider connection changed. Child tasks inherit the active parent model. Select the original profile/connection or configure subagent_models for the active connection; failed model requests are not automatically retried.".into(),
    })
}

fn env_value(env: &HashMap<String, String>, key: &str) -> String {
    env.get(key).cloned().unwrap_or_default()
}

/// Resolves `file`, `env`, `session`, and `overrides` into a [`Runtime`].
///
/// Precedence for provider/model: profile fields, then (only when no
/// explicit profile) `session`, then the `OTTO_PROVIDER`/`OTTO_MODEL`
/// environment variables, then `overrides`. `base_url` follows the same
/// chain but has no session or environment step beyond the profile.
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
    let explicit_profile = !overrides.profile.is_empty();
    file.approval.validate()?;
    let mut selected_profile = overrides.profile.clone();
    if selected_profile.is_empty() {
        selected_profile = env_value(env, "OTTO_PROFILE");
    }
    if selected_profile.is_empty() {
        selected_profile = file.default_profile.clone();
    }
    let runtime_profile = selected_profile.clone();

    let mut provider = String::new();
    let mut model = String::new();
    let mut thinking = String::new();
    let mut base_url = String::new();
    let mut api_key_env = String::new();
    let mut profile_config = Profile::default();

    if !selected_profile.is_empty() {
        let Some(profile) = file.profiles.get(&selected_profile) else {
            return Err(ConfigError::new(format!(
                "profile \"{selected_profile}\" not found"
            )));
        };
        profile_config = profile.clone();
        provider = profile.provider.clone();
        model = profile.model.clone();
        thinking = profile.thinking.clone();
        base_url = profile.base_url.clone();
        api_key_env = profile.api_key_env.clone();
    }
    if !explicit_profile {
        if !session.provider.is_empty() {
            provider = session.provider.clone();
        }
        if !session.model.is_empty() {
            model = session.model.clone();
        }
    }

    let env_provider = env_value(env, "OTTO_PROVIDER");
    if !env_provider.is_empty() {
        provider = env_provider;
    }
    let env_model = env_value(env, "OTTO_MODEL");
    if !env_model.is_empty() {
        model = env_model;
    }

    if !overrides.provider.is_empty() {
        provider = overrides.provider.clone();
    }
    if !overrides.model.is_empty() {
        model = overrides.model.clone();
    }
    if !overrides.thinking.is_empty() {
        thinking = overrides.thinking.clone();
    }
    if !overrides.base_url.is_empty() {
        base_url = overrides.base_url.clone();
    }

    if provider.is_empty() {
        return Err(ConfigError::new("missing provider"));
    }
    if provider != super::PROVIDER_OPENAI_COMPATIBLE && provider != super::PROVIDER_CHATGPT {
        return Err(ConfigError::new(format!(
            "unsupported provider \"{provider}\""
        )));
    }
    if model.is_empty() {
        return Err(ConfigError::new("missing model"));
    }
    validate_thinking(&thinking)?;

    if provider == super::PROVIDER_OPENAI_COMPATIBLE {
        if base_url.is_empty() {
            return Err(ConfigError::new("missing base_url"));
        }
        base_url = normalize_base_url(&base_url)
            .map_err(|err| ConfigError::new(format!("invalid base_url: {err}")))?;
    } else {
        // The chatgpt provider uses a fixed backend URL and OAuth credentials
        // from the credential file, not a configured base_url or API key.
        base_url = String::new();
    }
    validate_subagent_models(&selected_profile, &profile_config.subagent_models)?;
    let same_connection = provider == profile_config.provider
        && (provider == super::PROVIDER_CHATGPT
            || (api_key_env == profile_config.api_key_env
                && normalize_base_url(&profile_config.base_url).ok().as_deref()
                    == Some(base_url.as_str())));
    let subagent_models = if same_connection {
        profile_config.subagent_models.clone()
    } else {
        Vec::new()
    };

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

    let compaction = resolve_compaction(&file.agent.compaction, &profile_config, &model)?;
    let resilience = resolve_resilience(&file.agent)?;
    let progress = resolve_progress(&file.agent)?;

    let mut api_key = String::new();
    if provider == super::PROVIDER_OPENAI_COMPATIBLE && credentials {
        if let Some(allowed) = file
            .policy
            .as_ref()
            .and_then(|p| p.allowed_credential_variables.as_ref())
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
        api_key = resolve_api_key(env, &api_key_env)?;
    }

    let runtime = Runtime {
        profile: runtime_profile,
        provider,
        base_url,
        model,
        subagent_models,
        thinking,
        api_key,
        api_key_env,
        shell_timeout,
        max_output_bytes,
        compaction,
        resilience,
        progress,
    };
    if let Some(policy) = &file.policy {
        policy.runtime(&runtime)?;
    }
    Ok(runtime)
}

fn validate_subagent_models(profile: &str, models: &[String]) -> Result<(), ConfigError> {
    if models.len() > 16 {
        return Err(ConfigError::new(format!(
            "profile \"{profile}\" subagent_models must contain at most 16 IDs"
        )));
    }
    let mut seen = HashSet::new();
    for model in models {
        if model.trim() != model
            || model.is_empty()
            || model.len() > 256
            || model.chars().any(char::is_control)
        {
            return Err(ConfigError::new(format!(
                "profile \"{profile}\" subagent_models contains an invalid model ID"
            )));
        }
        if !seen.insert(model) {
            return Err(ConfigError::new(format!(
                "profile \"{profile}\" subagent_models contains a duplicate model ID"
            )));
        }
    }
    Ok(())
}

fn resolve_resilience(agent: &super::Agent) -> Result<ResilienceRuntime, ConfigError> {
    let deadlines = DeadlineRuntime {
        turn_timeout: optional_positive_duration("turn_timeout", &agent.turn_timeout)?,
        unattended_turn_timeout: unattended_turn_timeout(&agent.unattended_turn_timeout)?,
        provider_timeout: optional_positive_duration("provider_timeout", &agent.provider_timeout)?,
        cancellation_grace: optional_positive_duration(
            "cancellation_grace",
            &agent.cancellation_grace,
        )?
        .unwrap_or(DEFAULT_CANCELLATION_GRACE),
        subagent_timeout: optional_positive_duration("subagent_timeout", &agent.subagent_timeout)?,
        workflow_step_timeout: optional_positive_duration(
            "workflow_step_timeout",
            &agent.workflow_step_timeout,
        )?,
    };

    Ok(ResilienceRuntime { deadlines })
}

fn resolve_progress(agent: &super::Agent) -> Result<ProgressRuntime, ConfigError> {
    Ok(ProgressRuntime {
        repeated_calls: optional_progress_limit(
            "no_progress_repeats",
            agent.no_progress_repeats,
            DEFAULT_PROGRESS_REPEATED_CALLS,
        )?,
        consecutive_failures: optional_progress_limit(
            "no_progress_failures",
            agent.no_progress_failures,
            DEFAULT_PROGRESS_CONSECUTIVE_FAILURES,
        )?,
    })
}

/// An absent limit keeps the built-in default; an explicit zero disables the
/// condition.
fn optional_progress_limit(
    field: &str,
    configured: Option<i64>,
    default: u32,
) -> Result<u32, ConfigError> {
    let Some(value) = configured else {
        return Ok(default);
    };
    if !(0..=i64::from(MAX_PROGRESS_LIMIT)).contains(&value) {
        return Err(ConfigError::new(format!(
            "[agent].{field} must be between 0 and {MAX_PROGRESS_LIMIT}, got {value}"
        )));
    }
    Ok(value as u32)
}

fn optional_positive_duration(
    field: &str,
    configured: &Option<String>,
) -> Result<Option<Duration>, ConfigError> {
    let Some(value) = configured else {
        return Ok(None);
    };
    let nanos = parse_go_duration(value)
        .map_err(|err| ConfigError::new(format!("invalid {field}: {err}")))?;
    if nanos <= 0 {
        return Err(ConfigError::new(format!(
            "invalid {field}: must be greater than zero"
        )));
    }
    Ok(Some(Duration::from_nanos(nanos as u64)))
}

/// Omitted gives the default; a zero duration disables the deadline.
fn unattended_turn_timeout(configured: &Option<String>) -> Result<Option<Duration>, ConfigError> {
    let Some(value) = configured else {
        return Ok(Some(DEFAULT_UNATTENDED_TURN_TIMEOUT));
    };
    let nanos = parse_go_duration(value)
        .map_err(|err| ConfigError::new(format!("invalid unattended_turn_timeout: {err}")))?;
    if nanos < 0 {
        return Err(ConfigError::new(
            "invalid unattended_turn_timeout: must not be negative",
        ));
    }
    Ok((nanos > 0).then(|| Duration::from_nanos(nanos as u64)))
}

fn validate_thinking(thinking: &str) -> Result<(), ConfigError> {
    if matches!(thinking, "" | "low" | "medium" | "high" | "xhigh" | "max") {
        return Ok(());
    }
    Err(ConfigError::new(
        "invalid thinking: must be one of low, medium, high, xhigh, max",
    ))
}

fn resolve_compaction(
    config: &CompactionConfig,
    profile: &Profile,
    model: &str,
) -> Result<CompactionRuntime, ConfigError> {
    let auto = config.auto.unwrap_or(true);

    let reserve = match config.reserve_tokens {
        Some(value) if value <= 0 => {
            return Err(ConfigError::new(
                "invalid reserve_tokens: must be greater than zero",
            ));
        }
        Some(value) => value,
        None => DEFAULT_COMPACTION_RESERVE,
    };

    let keep = match config.keep_recent_tokens {
        Some(value) if value <= 0 => {
            return Err(ConfigError::new(
                "invalid keep_recent_tokens: must be greater than zero",
            ));
        }
        Some(value) => value,
        None => DEFAULT_COMPACTION_KEEP,
    };

    if let Some(context_window) = profile.context_window
        && context_window < MINIMUM_COMPACTION_WINDOW
    {
        return Err(ConfigError::new(format!(
            "invalid context_window: must be at least {MINIMUM_COMPACTION_WINDOW}"
        )));
    }
    if let Some(compaction_window) = profile.compaction_window {
        let Some(context_window) = profile.context_window else {
            return Err(ConfigError::new(
                "invalid context_window: required with compaction_window",
            ));
        };
        if compaction_window < MINIMUM_COMPACTION_WINDOW {
            return Err(ConfigError::new(format!(
                "invalid compaction_window: must be at least {MINIMUM_COMPACTION_WINDOW}"
            )));
        }
        if compaction_window > context_window {
            return Err(ConfigError::new(
                "invalid compaction_window: must not exceed context_window",
            ));
        }
    }

    let limits = resolve_model_limits(model);
    let mut runtime = CompactionRuntime {
        auto,
        context_window: limits.context_window,
        hard_input_window: limits.hard_input_window,
        working_window: limits.working_window,
        max_output_tokens: limits.max_output_tokens,
        reserve_tokens: reserve,
        keep_recent_tokens: keep,
    };
    if let Some(context_window) = profile.context_window {
        runtime.context_window = context_window;
        runtime.hard_input_window = context_window;
        runtime.working_window = context_window;
    }
    if let Some(compaction_window) = profile.compaction_window {
        runtime.working_window = compaction_window;
    }

    if runtime.working_window > 0 {
        let reserve_cap = MINIMUM_COMPACTION_TARGET.max(runtime.working_window / 4);
        runtime.reserve_tokens = runtime.reserve_tokens.min(reserve_cap);
        let keep_cap =
            MINIMUM_COMPACTION_TARGET.max((runtime.working_window - runtime.reserve_tokens) / 2);
        runtime.keep_recent_tokens = runtime.keep_recent_tokens.min(keep_cap);
    }
    Ok(runtime)
}

fn resolve_api_key(
    env: &HashMap<String, String>,
    api_key_env: &str,
) -> Result<String, ConfigError> {
    if !api_key_env.is_empty() {
        let value = env_value(env, api_key_env);
        if !value.is_empty() {
            return Ok(value);
        }
        let fallback = env_value(env, "OTTO_API_KEY");
        if !fallback.is_empty() {
            return Ok(fallback);
        }
        return Err(ConfigError::new("missing api key"));
    }
    let fallback = env_value(env, "OTTO_API_KEY");
    if !fallback.is_empty() {
        return Ok(fallback);
    }
    Err(ConfigError::new("missing api key"))
}

/// Re-implements `otto::provider::openaicompat::normalize_base_url` on the
/// `url` crate.
///
/// ponytail: this duplicates (rather than calls)
/// `otto::provider::openaicompat::normalize_base_url`, because otto-core cannot
/// depend on the native `otto` crate that owns it. The tests below pin every
/// accepted and rejected form, so the two implementations stay in sync.
pub(super) fn normalize_base_url(base_url: &str) -> Result<String, &'static str> {
    const INVALID: &str = "invalid OpenAI-compatible base URL";
    let mut parsed = Url::parse(base_url).map_err(|_| INVALID)?;
    let scheme_ok = parsed.scheme() == "http" || parsed.scheme() == "https";
    let host_ok = parsed.host_str().is_some_and(|host| !host.is_empty());
    if !scheme_ok
        || !host_ok
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || go_authority_is_empty(base_url)
    {
        return Err(INVALID);
    }
    let trimmed_path = parsed.path().trim_end_matches('/').to_string();
    parsed.set_path(&trimmed_path);
    Ok(parsed.to_string())
}

/// Reports whether Go's `net/url.Parse` would give an empty `Host` for
/// `raw`, i.e. whether the text right after the first `"://"` is empty or
/// starts with `/`, `?`, or `#`.
///
/// ponytail: needed because Rust's `url` crate (a WHATWG-spec parser) is
/// stricter about slash-runs than Go's parser: for `"http:///v1"`, Go's
/// lenient authority scan sees nothing between the second and third slash
/// and sets `Host == ""` (rejected by `NormalizeBaseURL`), while `url`
/// folds the leftover slash into the path and reads `"v1"` as the host
/// instead of rejecting it. Checking the raw text directly, rather than
/// the already-parsed `Url`, reproduces Go's rule for this one divergence.
fn go_authority_is_empty(raw: &str) -> bool {
    match raw.find("://") {
        Some(index) => match raw.as_bytes().get(index + 3) {
            None => true,
            Some(b'/' | b'?' | b'#') => true,
            Some(_) => false,
        },
        None => false,
    }
}

#[cfg(test)]
mod tests;
