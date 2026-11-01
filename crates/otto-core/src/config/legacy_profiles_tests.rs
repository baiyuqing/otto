//! Tests of the conversion from `[profiles.*]` to providers and models.

use super::{File, parse};

fn converted(text: &str) -> File {
    parse(text).expect("parse")
}

fn deprecation_reason(file: &File) -> String {
    let matches: Vec<_> = file
        .diagnostics
        .iter()
        .filter(|d| d.key == "profiles")
        .collect();
    assert_eq!(matches.len(), 1, "{:?}", file.diagnostics);
    matches[0].reason.clone()
}

#[test]
fn profile_with_builtin_connection_uses_the_builtin_provider() {
    let file = converted(
        "default_profile='main'\n[profiles.main]\nprovider='chatgpt'\nmodel='gpt-5'\nthinking='high'\n\
         [profiles.api]\nprovider='claude'\nmodel='sonnet'\nbase_url='https://api.anthropic.com'\n",
    );
    assert_eq!(file.default_model, "chatgpt/gpt-5");
    assert!(file.default_profile.is_empty());
    assert_eq!(file.providers["chatgpt"].models["gpt-5"].thinking, "high");
    assert!(file.providers["claude"].models.contains_key("sonnet"));
    assert!(file.providers["claude"].base_url.is_empty());
    assert_eq!(
        file.legacy_profile_selectors["api"].to_string(),
        "claude/sonnet"
    );
}

#[test]
fn claude_profile_with_custom_base_url_gets_its_own_provider() {
    let file = converted(
        "[profiles.plain]\nprovider='claude'\nmodel='a'\n\
         [profiles.proxy]\nprovider='claude'\nmodel='b'\nbase_url='https://proxy.example'\n",
    );
    assert!(file.providers["claude"].models.contains_key("a"));
    assert!(!file.providers["claude"].models.contains_key("b"));
    assert_eq!(file.providers["proxy"].type_, "claude");
    assert_eq!(file.providers["proxy"].base_url, "https://proxy.example");
    assert_eq!(
        file.legacy_profile_selectors["proxy"].to_string(),
        "proxy/b"
    );
}

#[test]
fn custom_provider_name_avoids_builtin_and_configured_names() {
    let file = converted(
        "[profiles.claude]\nprovider='claude'\nmodel='b'\nbase_url='https://proxy.example'\n",
    );
    assert_eq!(
        file.providers["claude-custom"].base_url,
        "https://proxy.example"
    );
    assert_eq!(
        file.legacy_profile_selectors["claude"].to_string(),
        "claude-custom/b"
    );
}

#[test]
fn profiles_with_the_same_connection_share_one_provider() {
    let file = converted(
        "[profiles.a]\nprovider='openai-compatible'\nmodel='m1'\nbase_url='https://x.example/v1'\napi_key_env='K'\n\
         [profiles.b]\nprovider='openai-compatible'\nmodel='m2'\nbase_url='https://x.example/v1'\napi_key_env='K'\n\
         [profiles.c]\nprovider='openai-compatible'\nmodel='m3'\nbase_url='https://y.example/v1'\napi_key_env='K'\n",
    );
    assert_eq!(file.providers["a"].models.len(), 2);
    assert_eq!(file.legacy_profile_selectors["b"].to_string(), "a/m2");
    assert_eq!(file.providers["c"].base_url, "https://y.example/v1");
}

#[test]
fn same_selector_with_different_settings_is_an_error_naming_both_profiles() {
    let error = parse(
        "[profiles.a]\nprovider='chatgpt'\nmodel='m'\nthinking='low'\n\
         [profiles.b]\nprovider='chatgpt'\nmodel='m'\nthinking='high'\n",
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("\"a\"") && error.contains("\"b\""),
        "{error}"
    );
    assert!(error.contains("chatgpt/m"), "{error}");
}

#[test]
fn identical_profiles_for_one_selector_are_accepted() {
    let file = converted(
        "[profiles.a]\nprovider='chatgpt'\nmodel='m'\n[profiles.b]\nprovider='chatgpt'\nmodel='m'\n",
    );
    assert_eq!(
        file.legacy_profile_selectors["a"],
        file.legacy_profile_selectors["b"]
    );
}

#[test]
fn reviewer_profile_becomes_reviewer_model() {
    let file = converted(
        "[approval]\nreviewer_profile='rev'\n[profiles.rev]\nprovider='chatgpt'\nmodel='mini'\n",
    );
    assert_eq!(
        file.approval.reviewer_model.as_deref(),
        Some("chatgpt/mini")
    );
    assert!(file.approval.reviewer_profile.is_none());
}

#[test]
fn dangling_default_profile_stays_for_resolution_to_report() {
    let file = converted("default_profile='gone'\n");
    assert_eq!(file.default_profile, "gone");
    assert!(file.default_model.is_empty());
}

#[test]
fn diagnostic_prints_the_equivalent_configuration_and_skipped_profiles() {
    let file = converted(
        "default_profile='main'\n[profiles.main]\nprovider='openai-compatible'\nmodel='m'\n\
         base_url='https://x.example/v1'\napi_key_env='K'\nthinking='low'\n\
         [profiles.empty]\nprovider='chatgpt'\n\
         [profiles.odd]\nprovider='codex'\nmodel='z'\n",
    );
    let reason = deprecation_reason(&file);
    assert!(reason.contains("default_model = \"main/m\""), "{reason}");
    assert!(reason.contains("[providers.main]"), "{reason}");
    assert!(
        reason.contains("models.\"m\" = { thinking = \"low\" }"),
        "{reason}"
    );
    assert!(!reason.contains("[providers.main.models"), "{reason}");
    assert!(
        reason.contains("base_url = \"https://x.example/v1\""),
        "{reason}"
    );
    assert!(
        reason.contains("# profiles.empty was not converted"),
        "{reason}"
    );
    assert!(
        reason.contains("# profiles.odd was not converted"),
        "{reason}"
    );
    assert!(!reason.contains("api_key ="), "{reason}");
}

#[test]
fn diagnostic_writes_built_in_providers_with_their_own_header() {
    let file = converted("[profiles.a]\nprovider='chatgpt'\nmodel='m'\n");
    let reason = deprecation_reason(&file);
    assert!(
        reason.contains("[providers.chatgpt]\ntype = \"chatgpt\"\nmodels.\"m\" = {}\n"),
        "{reason}"
    );
}

#[test]
fn unused_connection_fields_are_dropped_for_chatgpt() {
    let file = converted(
        "[profiles.sub]\nprovider='chatgpt'\nmodel='m'\nbase_url='https://ignored.example'\napi_key_env='IGNORED'\n",
    );
    assert!(file.providers["chatgpt"].base_url.is_empty());
    assert!(file.providers["chatgpt"].api_key_env.is_empty());
}

#[test]
fn subagent_models_are_carried_on_the_converted_model() {
    let file =
        converted("[profiles.a]\nprovider='chatgpt'\nmodel='m'\nsubagent_models=['cheap']\n");
    assert_eq!(
        file.providers["chatgpt"].models["m"].legacy_subagent_models,
        ["cheap"]
    );
}

#[test]
fn a_file_without_legacy_keys_is_unchanged_and_has_no_diagnostic() {
    let file = converted("default_model='chatgpt/m'\n");
    assert!(file.diagnostics.iter().all(|d| d.key != "profiles"));
}

#[test]
fn reviewer_model_is_validated_as_a_selector() {
    let file = converted("[approval]\nreviewer_model='chatgpt/mini'\n");
    assert_eq!(
        file.approval.reviewer_model.as_deref(),
        Some("chatgpt/mini")
    );
    let error = parse("[approval]\nreviewer_model='mini'\n")
        .unwrap_err()
        .to_string();
    assert!(error.contains("approval.reviewer_model"), "{error}");
}
