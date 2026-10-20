//! Resolve tests: profile, environment and CLI precedence, provider validation and API keys.

use super::*;

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
