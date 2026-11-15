//! Resolve tests: deadline, progress and timeout limits, and base URL normalization.

use super::*;

#[test]
fn resolves_deadline_defaults_and_explicit_values() {
    let file = file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))]);
    let runtime = resolve(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            model: selector_of(&file, "sub"),
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
            model: selector_of(&file, "sub"),
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

#[test]
fn resolves_progress_defaults_and_explicit_values() {
    let file = file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))]);
    let runtime = resolve(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides {
            model: selector_of(&file, "sub"),
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
            model: selector_of(&file, "sub"),
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

#[test]
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

#[test]
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
                    model: selector_of(&file, "sub"),
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

#[test]
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

#[test]
fn resolves_unattended_turn_timeout() {
    let resolve_with = |value: Option<&str>| {
        let mut file = file_with_profiles("", &[("sub", profile("chatgpt", "test-model", "", ""))]);
        file.agent.unattended_turn_timeout = value.map(Into::into);
        resolve(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides {
                model: selector_of(&file, "sub"),
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

#[test]
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
                    model: selector_of(&file, "sub"),
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(err.to_string().contains(field), "{field}={value:?}: {err}");
        }
    }
}

#[test]
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
                model: selector_of(&file, "local"),
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

#[test]
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
                model: selector_of(&file, "local"),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("base_url"), "{base_url}: {err}");
    }
}

#[test]
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
            model: selector_of(&file, "local"),
            ..Default::default()
        },
    )
    .expect("resolve");
    assert_eq!(runtime.base_url, "https://example.com/gateway/v1");
}
