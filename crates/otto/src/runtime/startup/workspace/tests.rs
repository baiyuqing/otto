use super::*;
use otto_core::config::ProviderEntry;
use std::cell::Cell;

fn loaded() -> Loaded {
    let config = File {
        default_model: "gateway/test".into(),
        providers: std::collections::BTreeMap::from([(
            "gateway".into(),
            ProviderEntry {
                type_: "openai-compatible".into(),
                base_url: "https://untrusted.example/v1".into(),
                api_key_env: "CUSTOM_CREDENTIAL".into(),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    Loaded {
        boundary: StartupBoundary {
            sandbox_secrets: vec![],
            complete: true,
            config: File::default(),
            environment: HashMap::new(),
            base_url: String::new(),
        },
        home: "/unused-home".into(),
        config_path: String::new(),
        config,
    }
}

fn check_readers(
    loaded: &mut Loaded,
    lookup: EnvironmentLookup,
    overrides: Overrides,
    accepted: bool,
) {
    let environment_reads = Cell::new(0);
    let auth_reads = Cell::new(0);
    let mut stderr = Vec::new();
    let result = configure_with_readers(
        loaded,
        &[],
        &lookup,
        &overrides,
        &mut stderr,
        |file, lookup| {
            environment_reads.set(environment_reads.get() + 1);
            config_environment(file, lookup)
        },
        |_| {
            auth_reads.set(auth_reads.get() + 1);
            CapturedAuth {
                complete: true,
                redaction_values: vec!["auth-file-secret".into()],
                ..Default::default()
            }
        },
    );
    assert_eq!(result.is_ok(), accepted);
    assert_eq!(environment_reads.get(), usize::from(accepted));
    assert_eq!(auth_reads.get(), usize::from(accepted));
    if let Err(error) = result {
        assert!(!error.contains("untrusted.example"));
        assert!(!error.contains("secret-marker"));
        assert!(stderr.is_empty());
    } else {
        assert!(
            !loaded
                .boundary
                .redact("auth-file-secret")
                .contains("auth-file-secret")
        );
    }
}

#[test]
fn untrusted_active_endpoint_never_reads_environment_or_auth() {
    check_readers(&mut loaded(), HashMap::new(), Overrides::default(), false);
}

#[test]
fn endpoint_override_is_checked_before_readers() {
    let mut loaded = loaded();
    loaded.config.default_model = "claude/test".into();
    check_readers(
        &mut loaded,
        HashMap::new(),
        Overrides {
            base_url: "https://untrusted.example/secret-marker".into(),
            ..Default::default()
        },
        false,
    );
}

#[test]
fn cli_selection_can_bypass_inactive_untrusted_default() {
    check_readers(
        &mut loaded(),
        HashMap::new(),
        Overrides {
            model: "claude/test".into(),
            ..Default::default()
        },
        true,
    );
}

#[test]
fn environment_selection_can_bypass_inactive_untrusted_default() {
    check_readers(
        &mut loaded(),
        HashMap::from([("OTTO_MODEL".into(), "claude/test".into())]),
        Overrides::default(),
        true,
    );
}

#[test]
fn selected_untrusted_environment_model_never_reads_credentials() {
    let mut loaded = loaded();
    loaded.config.default_model = "claude/test".into();
    check_readers(
        &mut loaded,
        HashMap::from([("OTTO_MODEL".into(), "gateway/test".into())]),
        Overrides::default(),
        false,
    );
}

#[test]
fn selection_errors_do_not_echo_arbitrary_configuration_text() {
    let mut loaded = loaded();
    loaded.config.default_model = "secret-marker".into();
    check_readers(&mut loaded, HashMap::new(), Overrides::default(), false);
}

#[test]
fn model_less_server_bootstrap_still_captures_redactions() {
    let mut loaded = loaded();
    loaded.config.default_model.clear();
    check_readers(&mut loaded, HashMap::new(), Overrides::default(), true);
}

#[test]
fn successful_preflight_preserves_custom_environment_redaction() {
    let mut loaded = loaded();
    loaded
        .config
        .providers
        .get_mut("gateway")
        .unwrap()
        .trusted_origins = vec!["https://untrusted.example".into()];
    check_readers(
        &mut loaded,
        HashMap::from([(
            "CUSTOM_CREDENTIAL".into(),
            "custom-credential-secret".into(),
        )]),
        Overrides::default(),
        true,
    );
    assert!(
        !loaded
            .boundary
            .redact("custom-credential-secret")
            .contains("custom-credential-secret")
    );
}
