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

mod compaction_defaults;
mod connection_precedence;
mod deadlines_progress;

#[test]
fn claude_defaults_and_workspace_credentials_are_environment_only() {
    let mut selected = profile("claude", "fixture-claude", "", "");
    selected.workspace_id_env = "CLAUDE_WORKSPACE".into();
    let file = file_with_profiles("selected", &[("selected", selected)]);
    let environment = HashMap::from([
        ("ANTHROPIC_API_KEY".into(), "fixture-key".into()),
        ("CLAUDE_WORKSPACE".into(), "fixture-workspace".into()),
    ]);
    let runtime = resolve(
        &file,
        &environment,
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .unwrap();
    assert_eq!(runtime.base_url, "https://api.anthropic.com/");
    assert_eq!(runtime.api_key_env, "ANTHROPIC_API_KEY");
    assert_eq!(runtime.api_version, "2026-10-21");
    assert_eq!(runtime.workspace_id, "fixture-workspace");
    let preview = resolve_without_credentials(
        &file,
        &environment,
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .unwrap();
    assert!(preview.api_key.is_empty());
    assert!(preview.workspace_id.is_empty());
}

#[test]
fn claude_workspace_binding_obeys_managed_credential_policy() {
    let text = r#"
default_profile = "claude"
[profiles.claude]
provider = "claude"
model = "fixture-claude"
workspace_id_env = "WORKSPACE_ID"
"#;
    let mut file = crate::config::parse(text).unwrap();
    file.policy = Some(crate::config::layered::ManagedPolicy {
        allowed_credential_variables: Some(vec!["ANTHROPIC_API_KEY".into()]),
        ..Default::default()
    });
    let environment = HashMap::from([("ANTHROPIC_API_KEY".into(), "fixture-key".into())]);
    let error = resolve(
        &file,
        &environment,
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("workspace credential binding"),
        "{error}"
    );
}

#[test]
fn claude_default_connection_keeps_subagent_models_and_checks_workspace_binding() {
    let mut selected = profile("claude", "fixture-claude", "", "");
    selected.subagent_models = vec!["fixture-child".into()];
    let file = file_with_profiles("selected", &[("selected", selected)]);
    let runtime = resolve_without_credentials(
        &file,
        &HashMap::new(),
        &SessionDefaults::default(),
        &Overrides::default(),
    )
    .unwrap();
    assert_eq!(runtime.subagent_models, ["fixture-child"]);
    let mut changed = runtime.clone();
    changed.subagent_models.clear();
    changed.workspace_id_env = "OTHER_WORKSPACE".into();
    assert!(subagent_model_diagnostic(&file, &changed).is_some());
}
