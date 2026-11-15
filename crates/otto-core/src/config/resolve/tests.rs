use super::*;

mod server_bootstrap;
use crate::agent::progress::{DEFAULT_CONSECUTIVE_FAILURES, DEFAULT_REPEATED_CALLS};
use crate::config::{Agent, CompactionConfig as Compaction, ModelEntry, ProviderEntry};

/// One fixture provider with one model.
struct Fixture {
    provider: String,
    model: String,
    base_url: String,
    api_key_env: String,
    workspace_id_env: String,
}

fn profile(provider: &str, model: &str, base_url: &str, api_key_env: &str) -> Fixture {
    Fixture {
        provider: provider.into(),
        model: model.into(),
        base_url: base_url.into(),
        api_key_env: api_key_env.into(),
        workspace_id_env: String::new(),
    }
}

/// Builds a file with one provider per fixture, named by the fixture's key and
/// holding the fixture's model. `default_name` selects the default model.
fn file_with_profiles(default_name: &str, fixtures: &[(&str, Fixture)]) -> File {
    let mut file = File::default();
    for (name, fixture) in fixtures {
        let mut entry = ProviderEntry {
            type_: fixture.provider.clone(),
            base_url: fixture.base_url.clone(),
            api_key_env: fixture.api_key_env.clone(),
            workspace_id_env: fixture.workspace_id_env.clone(),
            ..Default::default()
        };
        entry
            .models
            .insert(fixture.model.clone(), ModelEntry::default());
        file.providers.insert(name.to_string(), entry);
        if *name == default_name {
            file.default_model = format!("{name}/{}", fixture.model);
        }
    }
    grant_fixture_endpoints(&mut file);
    file
}

/// The selector of the only model of the fixture provider `name`.
fn selector_of(file: &File, name: &str) -> String {
    let model = file.providers[name].models.keys().next().expect("model");
    format!("{name}/{model}")
}

/// Sets the windows of the model that `compaction_test_file` configures.
fn set_windows(file: &mut File, context_window: Option<i64>, compaction_window: Option<i64>) {
    let model = file
        .providers
        .values_mut()
        .flat_map(|provider| provider.models.values_mut())
        .next()
        .expect("one model");
    model.context_window = context_window;
    model.compaction_window = compaction_window;
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
    assert_eq!(runtime.base_url, "https://api.anthropic.com");
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
default_model = "work/fixture-claude"
[providers.work]
type = "claude"
workspace_id_env = "WORKSPACE_ID"
models."fixture-claude" = {}
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

// Existing resolver behavior fixtures have explicit test-authority grants.
// Security policy tests call the production resolver directly in endpoint_tests.
fn grant_fixture_endpoints(file: &mut File) {
    for entry in file.providers.values_mut() {
        if let Ok(url) = url::Url::parse(&entry.base_url) {
            entry
                .trusted_origins
                .push(url.origin().ascii_serialization());
        }
        entry.trusted_origins.push("https://changed.example".into());
        entry.trusted_origins.push("https://other.example".into());
    }
}
