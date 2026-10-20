
use super::*;
use crate::agent::progress::{DEFAULT_CONSECUTIVE_FAILURES, DEFAULT_REPEATED_CALLS};
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
fn subagent_models_follow_only_the_configured_connection() {
    let mut selected = profile("chatgpt", "parent", "", "");
    selected.subagent_models = vec!["cheap".into(), "strong".into()];
    let file = file_with_profiles("selected", &[("selected", selected)]);
    let resolve_with = |overrides: Overrides| {
        resolve_without_credentials(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &overrides,
        )
        .expect("resolve")
    };
    assert_eq!(
        resolve_with(Overrides::default()).subagent_models,
        ["cheap", "strong"]
    );
    assert!(
        resolve_with(Overrides {
            provider: "openai-compatible".into(),
            base_url: "https://other.example/v1".into(),
            ..Overrides::default()
        })
        .subagent_models
        .is_empty()
    );

    let mut selected = profile(
        "openai-compatible",
        "parent",
        "https://example.com/v1",
        "KEY",
    );
    selected.subagent_models = vec!["cheap".into()];
    let file = file_with_profiles("selected", &[("selected", selected)]);
    let runtime = resolve_without_credentials(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            base_url: "https://other.example/v1".into(),
            ..Overrides::default()
        },
    )
    .expect("resolve");
    assert!(runtime.subagent_models.is_empty());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn subagent_model_fallback_diagnostic_is_connection_specific_and_safe() {
    let mut selected = profile(
        "openai-compatible",
        "private-parent",
        "https://private.example/v1",
        "PRIVATE_KEY",
    );
    selected.subagent_models = vec!["private-child".into()];
    let file = file_with_profiles("selected", &[("selected", selected)]);
    let resolved = |overrides: Overrides| {
        resolve_without_credentials(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &overrides,
        )
        .unwrap()
    };
    let same = resolved(Overrides::default());
    assert!(subagent_model_diagnostic(&file, &same).is_none());
    let model_only = resolved(Overrides {
        model: "other-parent".into(),
        ..Overrides::default()
    });
    assert!(subagent_model_diagnostic(&file, &model_only).is_none());
    let normalized = resolved(Overrides {
        base_url: "https://private.example/v1/".into(),
        ..Overrides::default()
    });
    assert!(subagent_model_diagnostic(&file, &normalized).is_none());
    let mut emptied = same.clone();
    emptied.subagent_models.clear();
    assert!(subagent_model_diagnostic(&file, &emptied).is_none());
    for overrides in [
        Overrides {
            base_url: "https://changed.example/v1".into(),
            ..Overrides::default()
        },
        Overrides {
            provider: "chatgpt".into(),
            ..Overrides::default()
        },
    ] {
        let runtime = resolved(overrides);
        let diagnostic = subagent_model_diagnostic(&file, &runtime).expect("fallback diagnostic");
        assert_eq!(diagnostic.key, "subagent_models");
        assert_eq!(diagnostic.source, "runtime");
        assert!(
            diagnostic
                .reason
                .contains("inherit the active parent model")
        );
        assert!(diagnostic.reason.contains("configure subagent_models"));
        assert!(diagnostic.reason.contains("not automatically retried"));
        let displayed = format!(
            "{} {} {}",
            diagnostic.key, diagnostic.source, diagnostic.reason
        );
        for private in ["private", "PRIVATE_KEY", "changed.example", "selected"] {
            assert!(!displayed.contains(private), "{displayed}");
        }
    }
    let mut no_list = file.clone();
    no_list
        .profiles
        .get_mut("selected")
        .unwrap()
        .subagent_models
        .clear();
    let runtime = resolve_without_credentials(
        &no_list,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            provider: "chatgpt".into(),
            ..Overrides::default()
        },
    )
    .unwrap();
    assert!(subagent_model_diagnostic(&no_list, &runtime).is_none());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn subagent_model_list_rejects_invalid_entries() {
    for models in [
        vec!["".into()],
        vec![" cheap".into()],
        vec!["cheap".into(), "cheap".into()],
        vec!["cheap".into(); 17],
    ] {
        let mut selected = profile("chatgpt", "parent", "", "");
        selected.subagent_models = models;
        let file = file_with_profiles("selected", &[("selected", selected)]);
        let error = resolve_without_credentials(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides::default(),
        )
        .expect_err("invalid list");
        assert!(
            error
                .to_string()
                .contains("profile \"selected\" subagent_models"),
            "{error}"
        );
    }
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
fn resolves_deadline_defaults_and_explicit_values() {
    let file = file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))]);
    let runtime = resolve(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            profile: "sub".into(),
            ..Default::default()
        },
    )
    .expect("resolve defaults");
    assert_eq!(runtime.resilience, ResilienceRuntime::default());

    let mut file = file;
    file.agent.turn_timeout = Some("1m".into());
    file.agent.provider_timeout = Some("10s".into());
    file.agent.cancellation_grace = Some("750ms".into());
    file.agent.subagent_timeout = Some("2h".into());
    file.agent.workflow_step_timeout = Some("3h".into());
    let runtime = resolve(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            profile: "sub".into(),
            ..Default::default()
        },
    )
    .expect("resolve explicit values");
    assert_eq!(
        runtime.resilience.deadlines,
        DeadlineRuntime {
            turn_timeout: Some(Duration::from_secs(60)),
            unattended_turn_timeout: Some(Duration::from_secs(30 * 60)),
            provider_timeout: Some(Duration::from_secs(10)),
            cancellation_grace: Duration::from_millis(750),
            subagent_timeout: Some(Duration::from_secs(2 * 60 * 60)),
            workflow_step_timeout: Some(Duration::from_secs(3 * 60 * 60)),
        }
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn resolves_progress_defaults_and_explicit_values() {
    let file = file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))]);
    let runtime = resolve(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            profile: "sub".into(),
            ..Default::default()
        },
    )
    .expect("resolve defaults");
    assert_eq!(runtime.progress, ProgressRuntime::default());

    let mut file = file;
    file.agent.no_progress_repeats = Some(3);
    file.agent.no_progress_failures = Some(0);
    let runtime = resolve(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            profile: "sub".into(),
            ..Default::default()
        },
    )
    .expect("resolve explicit values");
    assert_eq!(
        runtime.progress,
        ProgressRuntime {
            repeated_calls: 3,
            consecutive_failures: 0,
        }
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn progress_defaults_match_the_agents_own_defaults() {
    assert_eq!(
        ProgressRuntime::default().repeated_calls,
        DEFAULT_REPEATED_CALLS
    );
    assert_eq!(
        ProgressRuntime::default().consecutive_failures,
        DEFAULT_CONSECUTIVE_FAILURES
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn rejects_out_of_range_progress_limits() {
    for (field, values) in [
        ("no_progress_repeats", [-1_i64, 1001]),
        ("no_progress_failures", [-1_i64, 1001]),
    ] {
        for value in values {
            let mut file =
                file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))]);
            match field {
                "no_progress_repeats" => file.agent.no_progress_repeats = Some(value),
                "no_progress_failures" => file.agent.no_progress_failures = Some(value),
                _ => unreachable!(),
            }
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
            assert_eq!(
                err.to_string(),
                format!("[agent].{field} must be between 0 and 1000, got {value}")
            );
        }
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn deadline_defaults_match_the_public_contract() {
    assert_eq!(
        ResilienceRuntime::default(),
        ResilienceRuntime {
            deadlines: DeadlineRuntime {
                turn_timeout: None,
                unattended_turn_timeout: Some(Duration::from_secs(30 * 60)),
                provider_timeout: None,
                cancellation_grace: Duration::from_secs(5),
                subagent_timeout: None,
                workflow_step_timeout: None,
            },
        }
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn resolves_unattended_turn_timeout() {
    let resolve_with = |value: Option<&str>| {
        let mut file = file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))]);
        file.agent.unattended_turn_timeout = value.map(Into::into);
        resolve(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides {
                profile: "sub".into(),
                ..Default::default()
            },
        )
        .map(|runtime| runtime.resilience.deadlines.unattended_turn_timeout)
    };
    assert_eq!(resolve_with(None).unwrap(), Some(Duration::from_secs(1800)));
    assert_eq!(resolve_with(Some("0")).unwrap(), None);
    assert_eq!(resolve_with(Some("0s")).unwrap(), None);
    assert_eq!(
        resolve_with(Some("45m")).unwrap(),
        Some(Duration::from_secs(45 * 60))
    );
    for bad in ["-1s", "x"] {
        let err = resolve_with(Some(bad)).unwrap_err();
        assert!(
            err.to_string().contains("unattended_turn_timeout"),
            "{bad}: {err}"
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn rejects_invalid_deadline_durations() {
    let fields = [
        "turn_timeout",
        "provider_timeout",
        "cancellation_grace",
        "subagent_timeout",
        "workflow_step_timeout",
    ];
    for field in fields {
        for value in ["", "0s", "-1ns", "9223372036854775808ns"] {
            let mut file =
                file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))]);
            match field {
                "turn_timeout" => file.agent.turn_timeout = Some(value.into()),
                "provider_timeout" => file.agent.provider_timeout = Some(value.into()),
                "cancellation_grace" => file.agent.cancellation_grace = Some(value.into()),
                "subagent_timeout" => file.agent.subagent_timeout = Some(value.into()),
                "workflow_step_timeout" => {
                    file.agent.workflow_step_timeout = Some(value.into());
                }
                _ => unreachable!(),
            }
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
            assert!(err.to_string().contains(field), "{field}={value:?}: {err}");
        }
    }
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
