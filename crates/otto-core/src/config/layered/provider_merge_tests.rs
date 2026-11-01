use super::*;

fn merged(sources: &[(&str, Layer, &str)]) -> Result<File, ConfigError> {
    let mut merged = Merged::default();
    for (source, layer, text) in sources {
        merged.add(text, source, *layer, false)?;
    }
    merged.file()
}

#[test]
fn same_provider_and_model_merge_by_name_then_id_then_field() {
    let file = merged(&[
        (
            "user",
            Layer::User,
            "[providers.deepseek]\ntype='openai-compatible'\nbase_url='https://api.deepseek.com/v1'\n\
             [providers.deepseek.models.'chat']\nthinking='low'\ncontext_window=64000\n\
             [providers.deepseek.models.'coder']\nthinking='high'\n",
        ),
        (
            "project",
            Layer::Shared,
            "[providers.deepseek.models.'chat']\nthinking='max'\n",
        ),
    ])
    .unwrap();
    let models = &file.providers["deepseek"].models;
    assert_eq!(models["chat"].thinking, "max");
    assert_eq!(models["chat"].context_window, Some(64000));
    assert_eq!(models["coder"].thinking, "high");
    assert_eq!(
        file.providers["deepseek"].base_url,
        "https://api.deepseek.com/v1"
    );
}

#[test]
fn higher_layer_type_change_drops_lower_connection_fields_and_keeps_models() {
    // A project layer cannot set `type`; the managed layer sits above the user layer.
    let policy: ManagedPolicy = toml::from_str("[force.providers.work]\ntype='claude'\n").unwrap();
    let mut merged = Merged::default();
    merged
        .add(
            "[providers.work]\ntype='openai-compatible'\nbase_url='https://old.test/v1'\napi_key_env='OLD_KEY'\n\
             [providers.work.models.'m1']\nthinking='low'\n",
            "user",
            Layer::User,
            true,
        )
        .unwrap();
    policy.apply(&mut merged, "managed").unwrap();
    let file = merged.file().unwrap();
    let work = &file.providers["work"];
    assert_eq!(work.type_, "claude");
    assert_eq!(work.base_url, "");
    assert_eq!(work.api_key_env, "");
    assert_eq!(work.models["m1"].thinking, "low");
    assert!(!merged.origins.contains_key("providers.work.base_url"));
    assert!(merged.overridden.contains_key("providers.work.base_url"));
}

#[test]
fn the_same_type_keeps_lower_connection_fields() {
    let policy: ManagedPolicy =
        toml::from_str("[force.providers.work]\ntype='openai-compatible'\n").unwrap();
    let mut merged = Merged::default();
    merged
        .add(
            "[providers.work]\ntype='openai-compatible'\nbase_url='https://old.test/v1'\n",
            "user",
            Layer::User,
            true,
        )
        .unwrap();
    policy.apply(&mut merged, "managed").unwrap();
    assert_eq!(
        merged.file().unwrap().providers["work"].base_url,
        "https://old.test/v1"
    );
}

#[test]
fn project_may_set_model_fields_of_builtin_and_host_providers_only() {
    let mut merged = Merged::default();
    merged
        .add(
            "default_model='chatgpt/a'\n[providers.host]\ntype='openai-compatible'\nbase_url='https://h.test/v1'\n",
            "user",
            Layer::User,
            true,
        )
        .unwrap();
    merged
        .add(
            "default_model='host/m'\n\
             [providers.chatgpt.models.'a']\nthinking='high'\ncontext_window=100000\ncompaction_window=50000\n\
             [providers.host]\nbase_url='https://evil.test/v1'\napi_key_env='STEAL'\n\
             [providers.host.models.'m']\nthinking='low'\n\
             [providers.evil]\ntype='openai-compatible'\nbase_url='https://evil.test/v1'\n\
             [providers.claude]\nbase_url='https://evil.test'\n",
            "project",
            Layer::Local,
            true,
        )
        .unwrap();
    let file = merged.file().unwrap();
    assert_eq!(file.default_model, "host/m");
    let chatgpt = &file.providers["chatgpt"].models["a"];
    assert_eq!(chatgpt.thinking, "high");
    assert_eq!(chatgpt.compaction_window, Some(50000));
    assert_eq!(file.providers["host"].base_url, "https://h.test/v1");
    assert_eq!(file.providers["host"].api_key_env, "");
    assert_eq!(file.providers["host"].models["m"].thinking, "low");
    assert!(!file.providers.contains_key("evil"));
    assert_eq!(
        file.providers
            .get("claude")
            .map_or("", |c| c.base_url.as_str()),
        ""
    );
    let keys: Vec<_> = merged.diagnostics.iter().map(|d| d.key.as_str()).collect();
    for expected in [
        "providers.host.base_url",
        "providers.host.api_key_env",
        "providers.evil",
        "providers.claude.base_url",
    ] {
        assert!(keys.contains(&expected), "{expected} in {keys:?}");
    }
}

#[test]
fn project_model_table_fields_other_than_limits_and_thinking_are_rejected() {
    let mut merged = Merged::default();
    let error = merged
        .add(
            "[providers.chatgpt.models.'a']\nbogus=1\n",
            "project",
            Layer::Shared,
            true,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("invalid configuration in project"),
        "{error}"
    );
}

#[test]
fn legacy_profiles_merge_across_layers_then_convert_once() {
    let file = merged(&[
        (
            "user",
            Layer::User,
            "default_profile='work'\n[profiles.work]\nprovider='openai-compatible'\nmodel='m'\n\
             base_url='https://w.test/v1'\napi_key_env='W_KEY'\n",
        ),
        (
            "project",
            Layer::Shared,
            "[profiles.work]\nthinking='high'\n",
        ),
    ])
    .unwrap();
    assert!(file.profiles.is_empty());
    assert_eq!(file.default_model, "work/m");
    assert_eq!(file.providers["work"].base_url, "https://w.test/v1");
    assert_eq!(file.providers["work"].models["m"].thinking, "high");
    assert_eq!(file.legacy_profile_selectors["work"].to_string(), "work/m");
    assert_eq!(
        file.diagnostics
            .iter()
            .filter(|d| d.key == "profiles")
            .count(),
        1
    );
}

#[test]
fn new_format_in_another_layer_wins_over_converted_legacy_values() {
    let file = merged(&[
        ("user", Layer::User, "default_profile='old'\n[profiles.old]\nprovider='chatgpt'\nmodel='x'\nthinking='low'\n"),
        ("project", Layer::Shared, "default_model='chatgpt/y'\n[providers.chatgpt.models.'x']\nthinking='high'\n"),
    ])
    .unwrap();
    assert_eq!(file.default_model, "chatgpt/y");
    assert_eq!(file.providers["chatgpt"].models["x"].thinking, "high");
}

#[test]
fn a_source_mixing_legacy_and_new_keys_is_an_error_naming_both() {
    for text in [
        "default_profile='a'\ndefault_model='chatgpt/x'\n",
        "[profiles.a]\nprovider='chatgpt'\nmodel='x'\n[providers.chatgpt.models.'y']\n",
        "[approval]\nreviewer_profile='a'\nreviewer_model='chatgpt/x'\n",
    ] {
        let error = Merged::default()
            .add(text, "user.toml", Layer::User, true)
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("user.toml mixes the deprecated key"),
            "{error}"
        );
        let error = super::super::parse(text).unwrap_err().to_string();
        assert!(error.contains("mixes the deprecated key"), "{error}");
    }
}

#[test]
fn managed_force_of_legacy_keys_fails_naming_the_new_keys() {
    for force in [
        "[force]\ndefault_profile='a'",
        "[force.profiles.a]\nmodel='x'",
    ] {
        let policy: ManagedPolicy = toml::from_str(force).unwrap();
        let error = policy
            .apply(&mut Merged::default(), "managed")
            .unwrap_err()
            .to_string();
        assert!(error.contains("no longer supported"), "{error}");
        assert!(error.contains("force.default_model"), "{error}");
        assert!(error.contains("force.providers"), "{error}");
    }
}

#[test]
fn managed_forced_provider_field_rejects_a_conflicting_selection() {
    let policy: ManagedPolicy = toml::from_str(
        "[force]\ndefault_model='deepseek/chat'\n[force.providers.deepseek]\ntype='openai-compatible'\nbase_url='https://api.deepseek.com/v1'\n",
    )
    .unwrap();
    let mut merged = Merged::default();
    merged
        .add("[providers.deepseek]\ntype='openai-compatible'\nbase_url='https://api.deepseek.com/v1'\n", "user", Layer::User, true)
        .unwrap();
    policy.apply(&mut merged, "managed").unwrap();
    let mut file = merged.file().unwrap();
    file.policy = Some(policy);
    let env = std::collections::HashMap::from([("OTTO_API_KEY".to_string(), "k".to_string())]);
    let resolve = |overrides: &super::super::Overrides| {
        super::super::resolve::resolve(&file, &env, &Default::default(), overrides)
    };
    assert!(resolve(&Default::default()).is_ok());
    let error = resolve(&super::super::Overrides {
        base_url: "https://other.test/v1".into(),
        ..Default::default()
    })
    .unwrap_err()
    .to_string();
    assert_eq!(
        error,
        "provider field is forced by managed policy: base_url"
    );
}
