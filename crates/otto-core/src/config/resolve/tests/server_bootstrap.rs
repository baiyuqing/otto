use super::super::*;

#[test]
fn bootstrap_only_allows_absent_model_selection() {
    let file = File::default();
    let env = HashMap::new();
    let overrides = Overrides::default();
    assert!(resolve(&file, &env, &SessionDefaults::default(), &overrides).is_err());
    let runtime = resolve_server_bootstrap(&file, &env, &overrides).unwrap();
    assert!(runtime.model.is_empty());
    assert_eq!(runtime.shell_timeout, DEFAULT_SHELL_TIMEOUT);
    assert_eq!(runtime.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    assert_eq!(runtime.resilience, resolve_resilience(&file.agent).unwrap());
    let invalid = Overrides {
        model: "missing/model".into(),
        ..Default::default()
    };
    assert!(resolve_server_bootstrap(&file, &env, &invalid).is_err());
    let env = HashMap::from([("OTTO_MODEL".into(), "invalid".into())]);
    assert!(resolve_server_bootstrap(&file, &env, &overrides).is_err());
}

#[test]
fn bootstrap_preserves_configured_resolution() {
    let file = File::default();
    let env = HashMap::from([("TEST_BOOTSTRAP_KEY".into(), "offline-placeholder".into())]);
    let overrides = Overrides {
        model: "openai-compatible/test-model".into(),
        ..Default::default()
    };
    let configured = crate::config::parse(
        "[providers.openai-compatible]\n\
         api_key_env = 'TEST_BOOTSTRAP_KEY'\n\
         base_url = 'http://127.0.0.1:1'\n\
         trusted_origins = ['http://127.0.0.1:1']\n\
         allow_http_origins = ['http://127.0.0.1:1']\n",
    )
    .unwrap();
    let runtime = resolve_server_bootstrap(&configured, &env, &overrides)
        .expect("configured bootstrap resolves");
    let expected = resolve(&configured, &env, &SessionDefaults::default(), &overrides)
        .expect("ordinary configuration resolves");
    assert_eq!(runtime, expected);
    assert_eq!(runtime.model, "test-model");
    assert_eq!(runtime.base_url, "http://127.0.0.1:1");
    assert_eq!(runtime.api_key, "offline-placeholder");
    assert!(resolve_server_bootstrap(&file, &HashMap::new(), &overrides).is_err());
}

#[test]
fn bootstrap_validates_non_model_runtime_settings() {
    let mut file = File::default();
    file.agent.shell_timeout = "-1s".into();
    assert!(resolve_server_bootstrap(&file, &HashMap::new(), &Overrides::default()).is_err());
}
