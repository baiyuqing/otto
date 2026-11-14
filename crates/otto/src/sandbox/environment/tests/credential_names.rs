//! Tests for classification and redaction of credential variables.

use super::*;

#[test]
fn server_token_is_redacted_and_cannot_be_restored_to_tools() {
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: vec![b"OTTO_SERVER_TOKEN=test-server-listener-secret".to_vec()],
        allow_names: vec!["OTTO_SERVER_TOKEN".into()],
        ..Default::default()
    })
    .expect("environment");
    assert!(
        !snapshot
            .entries()
            .unwrap()
            .iter()
            .any(|entry| entry.starts_with("OTTO_SERVER_TOKEN="))
    );
    assert!(
        snapshot
            .redaction_values()
            .iter()
            .any(|value| value == "test-server-listener-secret")
    );
}

#[test]
fn a_failed_resolution_retains_bounded_redactions_from_every_valid_entry() {
    let (_guard, directories) = new_private_directories();
    let rejection = resolve_environment(&EnvironmentOptions {
        host_entries: entries(&[
            "AWS_SECRET_ACCESS_KEY=first-aws-secret",
            "HTTPS_PROXY=http://raw%20user:raw%2Fpass@[::1]:8443/path",
            "BROKEN",
            "AWS_SECRET_ACCESS_KEY=second-aws-secret",
        ]),
        private_directories: Some(directories.clone()),
        ..EnvironmentOptions::default()
    })
    .expect_err("a malformed entry rejects the environment");
    let snapshot = rejection.snapshot();
    assert!(snapshot.entries().is_none());
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "first-aws-secret",
            "second-aws-secret",
            "raw%20user:raw%2Fpass",
            "raw%20user",
            "raw%2Fpass",
            "raw user:raw/pass",
            "raw user",
            "raw/pass",
            &display_path(&directories.root),
            &display_path(&directories.temp),
            &display_path(&directories.cache),
        ],
    );
    assert_redactions_sorted(snapshot.redaction_values());
}

#[test]
fn provider_credentials_are_removed() {
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: entries(&[
            "OTTO_API_KEY=otto-sensitive-value",
            "otto_api_key=lower-otto-sensitive-value",
            "selected_key=selected-sensitive-value",
            "CONFIGURED_KEY=configured-sensitive-value",
            "ORDINARY=preserved",
        ]),
        provider_names: vec!["SELECTED_KEY".into(), "CONFIGURED_KEY".into()],
        allow_names: vec![
            "OTTO_API_KEY".into(),
            "otto_api_key".into(),
            "selected_key".into(),
            "CONFIGURED_KEY".into(),
        ],
        private_directories: None,
    })
    .unwrap();
    assert_environment_names(&snapshot, &["ORDINARY"]);
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "otto-sensitive-value",
            "lower-otto-sensitive-value",
            "selected-sensitive-value",
            "configured-sensitive-value",
        ],
    );
}

#[test]
fn loader_and_shell_injection_variables_are_never_restored() {
    let names = [
        "DYLD_INSERT_LIBRARIES",
        "dyld_custom",
        "LD_PRELOAD",
        "ld_library_path",
        "BASH_ENV",
        "env",
        "ZDOTDIR",
        "PROMPT_COMMAND",
        "CDPATH",
        "SHELLOPTS",
        "BASHOPTS",
    ];
    let mut host: Vec<Vec<u8>> = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}=classified-value-{index:02}").into_bytes())
        .collect();
    host.push(b"AUTHORS=preserved".to_vec());

    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        allow_names: names.iter().map(|name| (*name).to_owned()).collect(),
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_environment_names(&snapshot, &["AUTHORS"]);
    assert_eq!(snapshot.redaction_values().len(), names.len());
}

#[test]
fn internal_and_control_variables_are_never_restored() {
    let names = [
        "OTTO_SANDBOX",
        "otto_sandbox_profile_path",
        "SSH_AUTH_SOCK",
        "docker_host",
        "CONTAINER_HOST",
    ];
    let mut host: Vec<Vec<u8>> = Vec::new();
    let mut original_values: Vec<String> = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let value = format!("non-restorable-value-{index:02}");
        host.push(format!("{name}={value}").into_bytes());
        original_values.push(value);
    }
    host.push(b"SANDBOX_PROFILE=preserved".to_vec());

    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        allow_names: names.iter().map(|name| (*name).to_owned()).collect(),
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_environment_names(&snapshot, &["SANDBOX_PROFILE"]);
    let wanted: Vec<&str> = original_values.iter().map(String::as_str).collect();
    assert_redactions_contain(snapshot.redaction_values(), &wanted);
}

#[test]
fn sensitive_suffixes_are_classified_case_insensitively() {
    let names = [
        "BUILD_TOKEN",
        "build_secret",
        "DATABASE_PASSWORD",
        "database_passwd",
        "SERVICE_API_KEY",
        "service_access_key",
        "signing_private_key",
        "APP_CREDENTIAL",
        "app_credentials",
    ];
    let mut host: Vec<Vec<u8>> = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}=suffix-sensitive-{index:02}").into_bytes())
        .collect();
    host.push(b"AUTHORS=preserved".to_vec());
    host.push(b"PASSWORD_POLICY=preserved".to_vec());

    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_environment_names(&snapshot, &["AUTHORS", "PASSWORD_POLICY"]);
    assert_eq!(snapshot.redaction_values().len(), names.len());
}

#[test]
fn fixed_credential_names_are_classified() {
    let names = [
        "AWS_ACCESS_KEY_ID",
        "azure_client_id",
        "GCP_SERVICE_ACCOUNT_KEY",
        "github_pat",
        "GITLAB_CI_JOB_JWT",
        "NPM_CONFIG__AUTH",
        "REGISTRY_AUTH_FILE",
        "DOCKER_AUTH_CONFIG",
        "CI_JOB_JWT",
        "AUTHORIZATION",
        "http_cookie",
        "SSH_AUTH_SOCK",
        "DOCKER_HOST",
        "CONTAINER_HOST",
    ];
    let mut host: Vec<Vec<u8>> = names
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{name}=fixed-sensitive-{index:02}").into_bytes())
        .collect();
    host.push(b"AUTHORS=preserved".to_vec());

    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: host,
        ..EnvironmentOptions::default()
    })
    .unwrap();
    assert_environment_names(&snapshot, &["AUTHORS"]);
    assert_eq!(snapshot.redaction_values().len(), names.len());
}

#[test]
fn an_exact_allow_restores_only_automatic_non_provider_names() {
    let snapshot = resolve_environment(&EnvironmentOptions {
        host_entries: entries(&[
            "PROJECT_TOKEN=restored-sensitive-value",
            "project_token=case-sensitive-not-restored",
            "NPM_CONFIG__AUTH=restored-fixed-value",
            "PROVIDER_TOKEN=provider-sensitive-value",
            "BASH_ENV=shell-sensitive-value",
            "ORDINARY=preserved",
        ]),
        provider_names: vec!["PROVIDER_TOKEN".into()],
        allow_names: vec![
            "PROJECT_TOKEN".into(),
            "NPM_CONFIG__AUTH".into(),
            "PROVIDER_TOKEN".into(),
            "BASH_ENV".into(),
        ],
        private_directories: None,
    })
    .unwrap();
    assert_environment_names(
        &snapshot,
        &["NPM_CONFIG__AUTH", "ORDINARY", "PROJECT_TOKEN"],
    );
    assert_redactions_contain(
        snapshot.redaction_values(),
        &[
            "restored-sensitive-value",
            "case-sensitive-not-restored",
            "restored-fixed-value",
            "provider-sensitive-value",
            "shell-sensitive-value",
        ],
    );
}
