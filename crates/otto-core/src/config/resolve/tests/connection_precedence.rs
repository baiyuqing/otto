//! Resolve tests: model selection precedence, provider validation and API keys.

use super::*;

const TWO_PROVIDERS: &str = r#"
default_model = "alpha/shared"
[providers.alpha]
type = "openai-compatible"
base_url = "https://alpha.example/v1"
api_key_env = "ALPHA_KEY"
[providers.alpha.models."shared"]
thinking = "low"
[providers.alpha.models."alpha-only"]
[providers.beta]
type = "openai-compatible"
base_url = "https://beta.example/v1"
api_key_env = "BETA_KEY"
[providers.beta.models."shared"]
[providers.beta.models."vendor/with-slash"]
"#;

fn two_providers() -> File {
    let mut file = crate::config::parse(TWO_PROVIDERS).expect("parse");
    grant_fixture_endpoints(&mut file);
    file
}

fn keys() -> HashMap<String, String> {
    env_map(&[("ALPHA_KEY", "a-secret"), ("BETA_KEY", "b-secret")])
}

fn resolve_with(
    env: &HashMap<String, String>,
    session: SessionDefaults,
    overrides: Overrides,
) -> Result<Runtime, ConfigError> {
    resolve(&two_providers(), env, &session, &overrides)
}

#[test]
fn default_model_selects_provider_connection_and_model_thinking() {
    let runtime =
        resolve_with(&keys(), SessionDefaults::default(), Overrides::default()).expect("resolve");
    assert_eq!(runtime.model_selector.to_string(), "alpha/shared");
    assert_eq!(runtime.provider_name, "alpha");
    assert_eq!(runtime.provider_type, "openai-compatible");
    assert_eq!(runtime.model, "shared");
    assert_eq!(runtime.base_url, "https://alpha.example/v1");
    assert_eq!(runtime.api_key, "a-secret");
    assert_eq!(runtime.thinking, "low");
}

#[test]
fn same_model_id_under_two_providers_resolves_two_connections() {
    let runtime = resolve_with(
        &keys(),
        SessionDefaults::default(),
        Overrides {
            model: "beta/shared".into(),
            ..Default::default()
        },
    )
    .expect("resolve");
    assert_eq!(runtime.base_url, "https://beta.example/v1");
    assert_eq!(runtime.api_key, "b-secret");
    assert_eq!(runtime.thinking, "");
}

#[test]
fn model_id_may_contain_slashes() {
    let runtime = resolve_with(
        &keys(),
        SessionDefaults::default(),
        Overrides {
            model: "beta/vendor/with-slash".into(),
            ..Default::default()
        },
    )
    .expect("resolve");
    assert_eq!(runtime.provider_name, "beta");
    assert_eq!(runtime.model, "vendor/with-slash");
}

#[test]
fn precedence_is_cli_then_environment_then_session_then_profile_env_then_default_model() {
    let file = two_providers();
    let session = SessionDefaults {
        provider: "openai-compatible".into(),
        model: "session-model".into(),
        model_selector: "beta/alpha-session".into(),
    };
    let env = env_map(&[
        ("OTTO_MODEL", "beta/env-model"),
        ("ALPHA_KEY", "a"),
        ("BETA_KEY", "b"),
    ]);
    let cli = Overrides {
        model: "alpha/cli-model".into(),
        ..Default::default()
    };
    let runtime = resolve(&file, &env, &session, &cli).expect("cli");
    assert_eq!(runtime.model_selector.to_string(), "alpha/cli-model");
    let runtime = resolve(&file, &env, &session, &Overrides::default()).expect("env");
    assert_eq!(runtime.model_selector.to_string(), "beta/env-model");
    let env = env_map(&[("ALPHA_KEY", "a"), ("BETA_KEY", "b")]);
    let runtime = resolve(&file, &env, &session, &Overrides::default()).expect("session");
    assert_eq!(runtime.model_selector.to_string(), "beta/alpha-session");
    let runtime = resolve(
        &file,
        &env,
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .expect("default");
    assert_eq!(runtime.model_selector.to_string(), "alpha/shared");
}

#[test]
fn deprecated_provider_and_model_pairs_form_a_selector() {
    let runtime = resolve_with(
        &keys(),
        SessionDefaults::default(),
        Overrides {
            provider: "beta".into(),
            model: "shared".into(),
            ..Default::default()
        },
    )
    .expect("flags");
    assert_eq!(runtime.model_selector.to_string(), "beta/shared");
    let env = env_map(&[
        ("OTTO_PROVIDER", "beta"),
        ("OTTO_MODEL", "shared"),
        ("BETA_KEY", "b"),
    ]);
    let runtime =
        resolve_with(&env, SessionDefaults::default(), Overrides::default()).expect("env");
    assert_eq!(runtime.model_selector.to_string(), "beta/shared");
}

#[test]
fn bare_model_id_and_lone_provider_are_errors_with_a_hint() {
    let bare = resolve_with(
        &keys(),
        SessionDefaults::default(),
        Overrides {
            model: "shared".into(),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(bare.contains("--model <provider>/shared"), "{bare}");
    let lone = resolve_with(
        &keys(),
        SessionDefaults::default(),
        Overrides {
            provider: "beta".into(),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(lone.contains("--provider requires --model"), "{lone}");
    let env = env_map(&[("OTTO_PROVIDER", "beta")]);
    let lone_env = resolve_with(&env, SessionDefaults::default(), Overrides::default())
        .unwrap_err()
        .to_string();
    assert!(lone_env.contains("OTTO_PROVIDER"), "{lone_env}");
}

#[test]
fn unknown_provider_is_an_error() {
    let error = resolve_with(
        &keys(),
        SessionDefaults::default(),
        Overrides {
            model: "gamma/x".into(),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("unknown provider \"gamma\""), "{error}");
}

#[test]
fn missing_model_is_an_error() {
    let error = resolve(
        &File::default(),
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("missing model"), "{error}");
}

#[test]
fn old_session_uses_default_model_provider_of_the_same_type_else_builtin() {
    let session = |provider: &str| SessionDefaults {
        provider: provider.into(),
        model: "old-model".into(),
        ..Default::default()
    };
    let runtime = resolve_with(&keys(), session("openai-compatible"), Overrides::default())
        .expect("same type");
    assert_eq!(runtime.model_selector.to_string(), "alpha/old-model");
    let runtime = resolve_with(&keys(), session("chatgpt"), Overrides::default()).expect("builtin");
    assert_eq!(runtime.model_selector.to_string(), "chatgpt/old-model");
}

#[test]
fn model_without_a_table_uses_builtin_provider_defaults() {
    let runtime = resolve(
        &File::default(),
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            model: "chatgpt/gpt-5-codex".into(),
            ..Default::default()
        },
    )
    .expect("resolve");
    assert_eq!(runtime.provider_type, "chatgpt");
    assert_eq!(runtime.model, "gpt-5-codex");
    assert_eq!(runtime.base_url, "");
    assert_eq!(runtime.api_key, "");
}

#[test]
fn cli_thinking_wins_over_model_thinking() {
    let runtime = resolve_with(
        &keys(),
        SessionDefaults::default(),
        Overrides {
            thinking: "max".into(),
            ..Default::default()
        },
    )
    .expect("resolve");
    assert_eq!(runtime.thinking, "max");
}

#[test]
fn legacy_profile_flag_and_environment_select_converted_models() {
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
    let env = env_map(&[
        ("OTTO_PROFILE", "current"),
        ("CURRENT_KEY", "secret"),
        ("CONFIG_KEY", "c"),
    ]);
    let runtime = resolve(
        &file,
        &env,
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .expect("env");
    assert_eq!(runtime.model, "current-model");
    assert_eq!(runtime.base_url, "https://current.example/v1");
    let runtime = resolve(
        &file,
        &env,
        &SessionDefaults::default(),
        &Overrides {
            legacy_profile: "configured".into(),
            ..Default::default()
        },
    )
    .expect("flag");
    assert_eq!(runtime.model, "config-model");
    let error = resolve(
        &file,
        &env,
        &SessionDefaults::default(),
        &Overrides {
            legacy_profile: "missing".into(),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("profile \"missing\" not found"), "{error}");
}

#[test]
fn missing_named_api_key_environment_variable_still_resolves_the_model() {
    let runtime = resolve_with(
        &HashMap::new(),
        SessionDefaults::default(),
        Overrides::default(),
    )
    .expect("a missing key must not block startup or model selection");
    assert_eq!(runtime.model_selector.to_string(), "alpha/shared");
    assert_eq!(runtime.api_key_env, "ALPHA_KEY");
    assert!(runtime.api_key.is_empty());
}

#[test]
fn a_missing_key_is_not_a_policy_violating_credential_binding() {
    let mut file = two_providers();
    file.policy = Some(crate::config::layered::ManagedPolicy {
        allowed_credential_variables: Some(vec!["ALPHA_KEY".into()]),
        ..Default::default()
    });
    let runtime = resolve(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .expect("an unbound key binds no credential");
    assert!(runtime.api_key.is_empty());
    let error = resolve(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            model: "beta/shared".into(),
            ..Default::default()
        },
    );
    assert!(
        error.is_err(),
        "policy still rejects a disallowed variable name"
    );
}

#[test]
fn otto_api_key_is_the_fallback_when_no_key_variable_is_named() {
    let file = crate::config::parse(
        r#"default_model = "local/m"
[providers.local]
type = "openai-compatible"
base_url = "https://example.com/v1"
trusted_origins = ["https://example.com"]
"#,
    )
    .expect("parse");
    let runtime = resolve(
        &file,
        &env_map(&[("OTTO_API_KEY", "fallback-secret")]),
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .expect("resolve");
    assert_eq!(runtime.api_key, "fallback-secret");
}
