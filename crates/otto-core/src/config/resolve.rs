//! Precedence resolution: combines a parsed [`File`](super::File), an injected
//! environment map, session defaults, and CLI overrides into a [`Runtime`].
//!
//! `env` is read here by key (never via `std::env::var`, which core cannot
//! call): the native `otto::config` layer populates the map from the real
//! process environment before calling in.

use std::collections::HashMap;
use std::time::Duration;

use url::Url;

use super::{
    CompactionConfig, ConfigError, File, Profile, duration::parse_go_duration,
    model_limits::resolve_model_limits,
};

const DEFAULT_SHELL_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MAX_OUTPUT_BYTES: i64 = 51200;
const DEFAULT_COMPACTION_RESERVE: i64 = 16_384;
const DEFAULT_COMPACTION_KEEP: i64 = 20_000;
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

/// The fully resolved configuration for one process run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runtime {
    pub profile: String,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub thinking: String,
    /// The actual secret value, resolved from `env` by `api_key_env` (or the
    /// `OTTO_API_KEY` fallback). Empty for the `chatgpt` provider.
    pub api_key: String,
    pub api_key_env: String,
    pub shell_timeout: Duration,
    pub max_output_bytes: i64,
    pub compaction: CompactionRuntime,
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
    let explicit_profile = !overrides.profile.is_empty();
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

    let mut api_key = String::new();
    if provider == super::PROVIDER_OPENAI_COMPATIBLE {
        api_key = resolve_api_key(env, &api_key_env)?;
    }

    Ok(Runtime {
        profile: runtime_profile,
        provider,
        base_url,
        model,
        thinking,
        api_key,
        api_key_env,
        shell_timeout,
        max_output_bytes,
        compaction,
    })
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
fn normalize_base_url(base_url: &str) -> Result<String, &'static str> {
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
mod tests {
    use super::*;
    use crate::config::{Agent, CompactionConfig as Compaction};

    fn profile(provider: &str, model: &str, base_url: &str, api_key_env: &str) -> Profile {
        Profile {
            provider: provider.into(),
            model: model.into(),
            base_url: base_url.into(),
            api_key_env: api_key_env.into(),
            ..Default::default()
        }
    }

    fn file_with_profiles(default_profile: &str, profiles: &[(&str, Profile)]) -> File {
        File {
            default_profile: default_profile.into(),
            profiles: profiles
                .iter()
                .map(|(name, p)| (name.to_string(), p.clone()))
                .collect(),
            ..Default::default()
        }
    }

    fn compaction_test_file(model: &str) -> File {
        file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "openai-compatible",
                    model,
                    "https://example.com/v1",
                    "PROFILE_KEY",
                ),
            )],
        )
    }

    fn env_map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn precedence_prefers_explicit_profile_env_model_and_profile_base_url() {
        let file = file_with_profiles(
            "configured",
            &[
                (
                    "configured",
                    profile(
                        "openai-compatible",
                        "config-model",
                        "https://config.example/v1",
                        "CONFIG_KEY",
                    ),
                ),
                (
                    "explicit",
                    profile(
                        "openai-compatible",
                        "profile-model",
                        "https://profile.example/v1",
                        "PROFILE_KEY",
                    ),
                ),
            ],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("OTTO_MODEL", "env-model"), ("PROFILE_KEY", "secret")]),
            &SessionDefaults {
                provider: "openai-compatible".into(),
                model: "session-model".into(),
            },
            &Overrides {
                profile: "explicit".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.model, "env-model");
        assert_eq!(runtime.profile, "explicit");
        assert_eq!(runtime.base_url, "https://profile.example/v1");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn explicit_profile_overrides_resumed_provider_and_model() {
        let file = file_with_profiles(
            "",
            &[(
                "explicit",
                profile(
                    "openai-compatible",
                    "profile-model",
                    "https://example.com/v1",
                    "PROFILE_KEY",
                ),
            )],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults {
                provider: "codex".into(),
                model: "old-model".into(),
            },
            &Overrides {
                profile: "explicit".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.provider, "openai-compatible");
        assert_eq!(runtime.model, "profile-model");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn cli_overrides_win_over_environment() {
        let file = file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "openai-compatible",
                    "profile-model",
                    "https://example.com/v1",
                    "PROFILE_KEY",
                ),
            )],
        );
        let runtime = resolve(
            &file,
            &env_map(&[
                ("OTTO_PROVIDER", "openai-compatible"),
                ("OTTO_MODEL", "env-model"),
                ("PROFILE_KEY", "secret"),
            ]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                model: "cli-model".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.model, "cli-model");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn copies_thinking_override() {
        let file = file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "openai-compatible",
                    "profile-model",
                    "https://example.com/v1",
                    "PROFILE_KEY",
                ),
            )],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                thinking: "max".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.thinking, "max");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn profile_thinking_is_used_and_cli_override_wins() {
        let mut local = profile(
            "openai-compatible",
            "profile-model",
            "https://example.com/v1",
            "PROFILE_KEY",
        );
        local.thinking = "low".into();
        let file = file_with_profiles("", &[("local", local)]);
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.thinking, "low");

        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                thinking: "max".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.thinking, "max");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn environment_profile_wins_before_default_profile() {
        let file = file_with_profiles(
            "configured",
            &[
                (
                    "configured",
                    profile(
                        "openai-compatible",
                        "config-model",
                        "https://config.example/v1",
                        "CONFIG_KEY",
                    ),
                ),
                (
                    "current",
                    profile(
                        "openai-compatible",
                        "current-model",
                        "https://current.example/v1",
                        "CURRENT_KEY",
                    ),
                ),
            ],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("OTTO_PROFILE", "current"), ("CURRENT_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides::default(),
        )
        .expect("resolve");
        assert_eq!(runtime.profile, "current");
        assert_eq!(runtime.model, "current-model");
        assert_eq!(runtime.api_key, "secret");
        assert_eq!(runtime.base_url, "https://current.example/v1");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn explicit_profile_overrides_environment_profile() {
        let file = file_with_profiles(
            "configured",
            &[
                (
                    "env",
                    profile(
                        "openai-compatible",
                        "env-model",
                        "https://env.example/v1",
                        "ENV_KEY",
                    ),
                ),
                (
                    "explicit",
                    profile(
                        "openai-compatible",
                        "explicit-model",
                        "https://explicit.example/v1",
                        "EXPLICIT_KEY",
                    ),
                ),
            ],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("OTTO_PROFILE", "env"), ("EXPLICIT_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "explicit".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.profile, "explicit");
        assert_eq!(runtime.model, "explicit-model");
        assert_eq!(runtime.api_key, "secret");
        assert_eq!(runtime.base_url, "https://explicit.example/v1");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unknown_profile() {
        let err = resolve(
            &File::default(),
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides {
                profile: "missing".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_missing_model() {
        let file = file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "openai-compatible",
                    "",
                    "https://example.com/v1",
                    "PROFILE_KEY",
                ),
            )],
        );
        let err = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_unsupported_provider() {
        let file = file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "codex",
                    "test-model",
                    "https://example.com/v1",
                    "PROFILE_KEY",
                ),
            )],
        );
        let err = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("provider"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn chatgpt_provider_needs_no_base_url_or_key() {
        let file = file_with_profiles("", &[("sub", profile("chatgpt", "gpt-5-codex", "", ""))]);
        let runtime = resolve(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides {
                profile: "sub".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.provider, "chatgpt");
        assert_eq!(runtime.model, "gpt-5-codex");
        assert_eq!(runtime.base_url, "");
        assert_eq!(runtime.api_key, "");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn chatgpt_still_requires_model() {
        let file = file_with_profiles("", &[("sub", profile("chatgpt", "", "", ""))]);
        let err = resolve(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides {
                profile: "sub".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_missing_named_api_key_environment_variable() {
        let file = file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "openai-compatible",
                    "test-model",
                    "https://example.com/v1",
                    "PROFILE_KEY",
                ),
            )],
        );
        let err = resolve(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("api key"), "{err}");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn uses_otto_api_key_fallback() {
        let file = file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "openai-compatible",
                    "test-model",
                    "https://example.com/v1",
                    "",
                ),
            )],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("OTTO_API_KEY", "fallback-secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.api_key, "fallback-secret");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_invalid_shell_timeout() {
        for timeout in ["not-a-duration", "0s", "-1s"] {
            let mut file = file_with_profiles(
                "",
                &[(
                    "local",
                    profile(
                        "openai-compatible",
                        "test-model",
                        "https://example.com/v1",
                        "PROFILE_KEY",
                    ),
                )],
            );
            file.agent = Agent {
                shell_timeout: timeout.into(),
                ..Default::default()
            };
            let err = resolve(
                &file,
                &env_map(&[("PROFILE_KEY", "secret")]),
                &SessionDefaults::default(),
                &Overrides {
                    profile: "local".into(),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("shell_timeout"),
                "{timeout}: {err}"
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rejects_base_urls_rejected_by_openai_client() {
        for base_url in [
            "https://example.com/v1?tenant=x",
            "https://example.com/v1?",
            "https://example.com/v1#fragment",
            "https://username@example.com/v1",
            "https://username:password@example.com/v1",
            "ftp://example.com/v1",
            "http:///v1",
            "http://[::1",
        ] {
            let file = file_with_profiles(
                "",
                &[(
                    "local",
                    profile("openai-compatible", "test-model", base_url, "PROFILE_KEY"),
                )],
            );
            let err = resolve(
                &file,
                &env_map(&[("PROFILE_KEY", "secret")]),
                &SessionDefaults::default(),
                &Overrides {
                    profile: "local".into(),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(err.to_string().contains("base_url"), "{base_url}: {err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn normalizes_base_url_like_openai_client() {
        let file = file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "openai-compatible",
                    "test-model",
                    "https://example.com/gateway/v1/",
                    "PROFILE_KEY",
                ),
            )],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.base_url, "https://example.com/gateway/v1");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn session_defaults_win_over_default_profile() {
        let file = file_with_profiles(
            "configured",
            &[(
                "configured",
                profile(
                    "openai-compatible",
                    "config-model",
                    "https://config.example/v1",
                    "CONFIG_KEY",
                ),
            )],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("CONFIG_KEY", "secret")]),
            &SessionDefaults {
                provider: "openai-compatible".into(),
                model: "session-model".into(),
            },
            &Overrides::default(),
        )
        .expect("resolve");
        assert_eq!(runtime.model, "session-model");
        assert_eq!(runtime.base_url, "https://config.example/v1");
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_applies_defaults_and_explicit_auto() {
        let mut file = compaction_test_file("gpt-5.6-sol");
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(
            runtime.compaction,
            CompactionRuntime {
                auto: true,
                context_window: 1_050_000,
                hard_input_window: 922_000,
                working_window: 272_000,
                max_output_tokens: 128_000,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
            }
        );

        file.agent.compaction.auto = Some(false);
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert!(!runtime.compaction.auto);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_rejects_non_positive_targets_before_api_key_resolution() {
        let cases: &[(Compaction, &str)] = &[
            (
                Compaction {
                    reserve_tokens: Some(0),
                    ..Default::default()
                },
                "reserve_tokens",
            ),
            (
                Compaction {
                    reserve_tokens: Some(-1),
                    ..Default::default()
                },
                "reserve_tokens",
            ),
            (
                Compaction {
                    keep_recent_tokens: Some(0),
                    ..Default::default()
                },
                "keep_recent_tokens",
            ),
            (
                Compaction {
                    keep_recent_tokens: Some(-1),
                    ..Default::default()
                },
                "keep_recent_tokens",
            ),
        ];
        for (compaction, field) in cases {
            let mut file = compaction_test_file("private-model");
            file.agent.compaction = compaction.clone();
            let err = resolve(
                &file,
                &HashMap::new(),
                &SessionDefaults::default(),
                &Overrides {
                    profile: "local".into(),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(err.to_string().contains(field), "{field}: {err}");
            assert!(!err.to_string().contains("api key"), "{field}: {err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_applies_profile_overrides_after_model_precedence() {
        let mut file = compaction_test_file("gpt-5.6-sol");
        let mut local = file.profiles["local"].clone();
        local.context_window = Some(131_072);
        local.compaction_window = Some(100_000);
        file.profiles.insert("local".into(), local);

        let runtime = resolve(
            &file,
            &env_map(&[
                ("OTTO_MODEL", "private-deployment"),
                ("PROFILE_KEY", "secret"),
            ]),
            &SessionDefaults {
                model: "session-model".into(),
                ..Default::default()
            },
            &Overrides {
                profile: "local".into(),
                model: "cli-private-deployment".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.model, "cli-private-deployment");
        assert_eq!(
            runtime.compaction,
            CompactionRuntime {
                auto: true,
                context_window: 131_072,
                hard_input_window: 131_072,
                working_window: 100_000,
                max_output_tokens: 0,
                reserve_tokens: 16_384,
                keep_recent_tokens: 20_000,
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_context_override_replaces_catalog_windows() {
        let mut file = compaction_test_file("gpt-5.6-sol");
        let mut local = file.profiles["local"].clone();
        local.context_window = Some(65_536);
        file.profiles.insert("local".into(), local);

        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.compaction.context_window, 65_536);
        assert_eq!(runtime.compaction.hard_input_window, 65_536);
        assert_eq!(runtime.compaction.working_window, 65_536);
        assert_eq!(runtime.compaction.max_output_tokens, 128_000);
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_rejects_invalid_profile_windows() {
        let cases: &[(Option<i64>, Option<i64>, &str)] = &[
            (Some(0), None, "context_window"),
            (Some(-1), None, "context_window"),
            (Some(4_095), None, "context_window"),
            (Some(8_192), Some(0), "compaction_window"),
            (Some(8_192), Some(-1), "compaction_window"),
            (Some(8_192), Some(4_095), "compaction_window"),
            (None, Some(4_096), "context_window"),
            (Some(4_096), Some(4_097), "compaction_window"),
        ];
        for (context, compaction, field) in cases {
            let mut file = compaction_test_file("gpt-4o");
            let mut local = file.profiles["local"].clone();
            local.context_window = *context;
            local.compaction_window = *compaction;
            file.profiles.insert("local".into(), local);
            let err = resolve(
                &file,
                &HashMap::new(),
                &SessionDefaults::default(),
                &Overrides {
                    profile: "local".into(),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(err.to_string().contains(field), "{field}: {err}");
            assert!(!err.to_string().contains("api key"), "{field}: {err}");
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_adjusts_targets_for_small_windows() {
        for (window, reserve, keep) in [(4_096_i64, 1_024_i64, 1_536_i64), (8_192, 2_048, 3_072)] {
            let mut file = compaction_test_file("private-model");
            let mut local = file.profiles["local"].clone();
            local.context_window = Some(window);
            file.profiles.insert("local".into(), local);
            let runtime = resolve(
                &file,
                &env_map(&[("PROFILE_KEY", "secret")]),
                &SessionDefaults::default(),
                &Overrides {
                    profile: "local".into(),
                    ..Default::default()
                },
            )
            .expect("resolve");
            assert_eq!(
                runtime.compaction.reserve_tokens, reserve,
                "window {window}"
            );
            assert_eq!(
                runtime.compaction.keep_recent_tokens, keep,
                "window {window}"
            );
        }
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn compaction_unknown_model_preserves_configured_targets() {
        let mut file = compaction_test_file("private-model");
        file.agent.compaction.reserve_tokens = Some(99_999);
        file.agent.compaction.keep_recent_tokens = Some(88_888);
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(
            runtime.compaction,
            CompactionRuntime {
                auto: true,
                reserve_tokens: 99_999,
                keep_recent_tokens: 88_888,
                ..Default::default()
            }
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn applies_agent_defaults() {
        let file = file_with_profiles(
            "",
            &[(
                "local",
                profile(
                    "openai-compatible",
                    "test-model",
                    "https://example.com/v1",
                    "PROFILE_KEY",
                ),
            )],
        );
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                profile: "local".into(),
                ..Default::default()
            },
        )
        .expect("resolve");
        assert_eq!(runtime.shell_timeout, Duration::from_secs(120));
        assert_eq!(runtime.max_output_bytes, 51200);
    }
}
