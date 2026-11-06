use super::endpoint::TrustedEndpoint;
use super::resolve::{Overrides, SessionDefaults, resolve};
use super::{File, ProviderEntry};
use std::collections::HashMap;

fn custom_file() -> File {
    let mut file = File {
        default_model: "gateway/fixture-model".into(),
        ..Default::default()
    };
    file.providers.insert(
        "gateway".into(),
        ProviderEntry {
            type_: "openai-compatible".into(),
            base_url: "https://gateway.example/v1".into(),
            api_key_env: "FIXTURE_KEY".into(),
            ..Default::default()
        },
    );
    file
}

#[test]
fn endpoint_upgrade_and_override_fail_before_missing_credential_error() {
    let mut file = custom_file();
    let run = |file: &File, overrides: &Overrides| {
        resolve(
            file,
            &HashMap::new(),
            &SessionDefaults::default(),
            overrides,
        )
    };
    let error = run(&file, &Overrides::default()).unwrap_err().to_string();
    assert!(
        error.contains("not trusted") && error.contains("trusted_origins"),
        "{error}"
    );
    file.providers.get_mut("gateway").unwrap().trusted_origins =
        vec!["https://gateway.example".into()];
    let error = run(&file, &Overrides::default()).unwrap_err().to_string();
    assert!(
        !error.contains("not trusted"),
        "authorized endpoint should reach credential lookup: {error}"
    );
    let error = run(
        &file,
        &Overrides {
            base_url: "https://other.example/v1".into(),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("not trusted"), "{error}");
}

#[test]
fn endpoint_grants_round_trip_and_project_cannot_add_them() {
    use super::layered::{Layer, Merged};
    let mut merged = Merged::default();
    merged.add("[providers.gateway]\ntype='openai-compatible'\nbase_url='https://gateway.example/v1'\n", "user", Layer::User, false).unwrap();
    merged.add("[providers.gateway]\ntrusted_origins=['https://gateway.example']\nallow_http_origins=['http://127.0.0.1']\n", "project", Layer::Shared, false).unwrap();
    let file = merged.file().unwrap();
    assert!(file.providers["gateway"].trusted_origins.is_empty());
    assert!(file.providers["gateway"].allow_http_origins.is_empty());
    assert!(
        merged
            .diagnostics
            .iter()
            .any(|entry| entry.key.ends_with("trusted_origins"))
    );
    let entry = ProviderEntry {
        trusted_origins: vec!["https://gateway.example".into()],
        allow_http_origins: vec!["http://127.0.0.1".into()],
        ..Default::default()
    };
    let decoded: ProviderEntry = toml::from_str(&toml::to_string(&entry).unwrap()).unwrap();
    assert_eq!(entry, decoded);
}

#[test]
fn endpoint_capabilities_are_provider_bound() {
    assert!(
        TrustedEndpoint::authorize("https://api.anthropic.com", "openai-compatible", &[], &[])
            .is_err()
    );
    assert!(TrustedEndpoint::authorize("https://api.openai.com", "claude", &[], &[]).is_err());
}

#[test]
fn endpoint_model_grants_and_invalid_inactive_urls_are_rejected() {
    assert!(
        super::parse(
            "[providers.claude.models.fixture]\ntrusted_origins=['https://evil.example']\n"
        )
        .is_err()
    );
    let error = super::parse("default_model='chatgpt/fixture'\n[providers.inactive]\ntype='openai-compatible'\nbase_url='https://example.com?secret'\n").unwrap_err().to_string();
    assert!(!error.contains("secret"));
    assert!(error.contains("invalid provider endpoint"));
    assert!(
        super::parse("[providers.claude]\ntrusted_origins=['https://gateway.example/path']\n")
            .is_err()
    );
}

#[test]
fn endpoint_managed_grants_authorize_without_project_authority() {
    use super::layered::{ManagedPolicy, Merged};
    let policy: ManagedPolicy = toml::from_str(concat!(
        "[force]\ndefault_model='gateway/fixture'\n[force.providers.gateway]\n",
        "type='openai-compatible'\nbase_url='https://gateway.example/v1'\n",
        "trusted_origins=['https://gateway.example']\n"
    ))
    .unwrap();
    let mut merged = Merged::default();
    policy.apply(&mut merged, "managed").unwrap();
    let file = merged.file().unwrap();
    assert!(
        super::resolve::resolve_without_credentials(
            &file,
            &HashMap::new(),
            &SessionDefaults::default(),
            &Overrides::default()
        )
        .is_ok()
    );
}
