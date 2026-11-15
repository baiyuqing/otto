//! Resolve tests: session defaults, compaction settings and agent defaults.

use super::*;

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
            ..Default::default()
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
            model: selector_of(&file, "local"),
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
            model: selector_of(&file, "local"),
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
                model: selector_of(&file, "local"),
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
fn compaction_applies_model_table_windows_over_catalog_windows() {
    let mut file = compaction_test_file("gpt-5.6-sol");
    set_windows(&mut file, Some(131_072), Some(100_000));

    let runtime = resolve(
        &file,
        &env_map(&[("PROFILE_KEY", "secret")]),
        &SessionDefaults::default(),
        &Overrides {
            model: selector_of(&file, "local"),
            ..Default::default()
        },
    )
    .expect("resolve");
    assert_eq!(runtime.model, "gpt-5.6-sol");
    assert_eq!(
        runtime.compaction,
        CompactionRuntime {
            auto: true,
            context_window: 131_072,
            hard_input_window: 131_072,
            working_window: 100_000,
            max_output_tokens: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn compaction_context_override_replaces_catalog_windows() {
    let mut file = compaction_test_file("gpt-5.6-sol");
    set_windows(&mut file, Some(65_536), None);

    let runtime = resolve(
        &file,
        &env_map(&[("PROFILE_KEY", "secret")]),
        &SessionDefaults::default(),
        &Overrides {
            model: selector_of(&file, "local"),
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
        set_windows(&mut file, *context, *compaction);
        let err = resolve(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides {
                model: selector_of(&file, "local"),
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
        set_windows(&mut file, Some(window), None);
        let runtime = resolve(
            &file,
            &env_map(&[("PROFILE_KEY", "secret")]),
            &SessionDefaults::default(),
            &Overrides {
                model: selector_of(&file, "local"),
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
            model: selector_of(&file, "local"),
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
            model: selector_of(&file, "local"),
            ..Default::default()
        },
    )
    .expect("resolve");
    assert_eq!(runtime.shell_timeout, Duration::from_secs(120));
    assert_eq!(runtime.max_output_bytes, 51200);
}
