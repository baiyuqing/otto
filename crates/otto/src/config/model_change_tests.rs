use std::fs;
use std::path::{Path, PathBuf};

use super::*;

const CONFIG: &str = "# retained\ndefault_model = \"local/big\"\n\n\
    [providers.local]\ntype = \"openai-compatible\"\nbase_url = \"https://example.test/v1\"\n\
    api_key_env = \"LOCAL_KEY\"\n\n\
    [providers.local.models.big]\nthinking = \"low\" # cheap\n\n\
    [providers.local.models.small]\n";

fn write_config(dir: &Path, content: &str) -> PathBuf {
    let path = dir.join("config.toml");
    fs::write(&path, content).expect("write fixture");
    path
}

fn preview(path: &Path, change: &ModelChange) -> Result<ModelChangePreview, ModelChangeError> {
    let unrelated_default = path.with_file_name("unrelated-default.toml");
    preview_model_change_impl(path, &unrelated_default, change)
}

fn updated_text(preview: &ModelChangePreview) -> String {
    String::from_utf8(preview.updated.clone()).expect("utf8")
}

#[test]
fn create_initial_config_refuses_an_existing_empty_file() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = write_config(directory.path(), "");

    let error = create_initial_config(&path, PROVIDER_CHATGPT, "my-model", "", "")
        .expect_err("must not replace an existing config");

    assert_eq!(error, "configuration already exists; no changes made");
    assert_eq!(fs::read(&path).expect("read"), b"");
}

#[test]
fn create_initial_config_writes_a_default_model_and_only_non_secret_fields() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("config.toml");

    create_initial_config(
        &path,
        PROVIDER_OPENAI_COMPATIBLE,
        "cheap-model",
        "https://api.example/v1",
        "EXAMPLE_KEY",
    )
    .expect("create");

    let content = fs::read_to_string(&path).expect("read");
    assert!(
        content.contains("default_model = \"openai-compatible/cheap-model\""),
        "{content}"
    );
    assert!(
        content.contains("api_key_env = \"EXAMPLE_KEY\""),
        "{content}"
    );
    assert!(!content.contains("profile"), "{content}");
    let parsed = otto_core::config::parse(&content).expect("parse");
    let provider = &parsed.providers["openai-compatible"];
    assert_eq!(provider.base_url, "https://api.example/v1");
    assert!(provider.models.contains_key("cheap-model"));
}

#[test]
fn a_legacy_profiles_config_still_loads_as_providers_and_a_default_model() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        directory.path(),
        "default_profile = \"work\"\n[profiles.work]\nprovider = \"openai-compatible\"\nmodel = \"m1\"\n\
         base_url = \"https://example.test/v1\"\napi_key_env = \"WORK_KEY\"\nthinking = \"high\"\n",
    );

    let file = load_required(&path).expect("legacy config loads");

    assert_eq!(file.default_model, "work/m1");
    assert_eq!(file.providers["work"].models["m1"].thinking, "high");
}

#[test]
fn set_default_model_changes_only_the_value_and_backs_up_the_old_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = write_config(dir.path(), CONFIG);

    set_default_model_file_impl(&path, &default, "local/small").expect("set default model");

    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        CONFIG.replace("\"local/big\"", "\"local/small\"")
    );
    let backups: Vec<_> = fs::read_dir(dir.path().join("backups"))
        .expect("backups")
        .collect();
    assert_eq!(backups.len(), 1);
}

#[test]
fn set_default_model_accepts_a_built_in_provider_without_a_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = write_config(dir.path(), CONFIG);

    set_default_model_file_impl(&path, &default, "chatgpt/gpt-x").expect("built-in provider");

    let content = fs::read_to_string(&path).expect("read back");
    assert!(
        content.contains("default_model = \"chatgpt/gpt-x\"\n"),
        "{content:?}"
    );
}

#[test]
fn set_default_model_rejects_an_unknown_provider_and_a_bare_model_id() {
    let dir = tempfile::tempdir().expect("tempdir");
    let default = dir.path().join("unrelated-default.toml");
    let path = write_config(dir.path(), CONFIG);

    let unknown = set_default_model_file_impl(&path, &default, "missing/m").unwrap_err();
    assert!(
        unknown
            .to_string()
            .contains("provider \"missing\" not found"),
        "{unknown}"
    );
    let bare = set_default_model_file_impl(&path, &default, "big").unwrap_err();
    assert!(bare.to_string().contains("<provider>/<model_id>"), "{bare}");
    assert_eq!(fs::read_to_string(&path).expect("read"), CONFIG);
}

#[test]
fn model_thinking_edit_changes_only_the_value_and_creates_a_missing_table() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), CONFIG);

    set_model_thinking_file(&path, "local/big", "high").expect("replace value");
    let replaced = CONFIG.replace("\"low\" #", "\"high\" #");
    assert_eq!(fs::read_to_string(&path).expect("read back"), replaced);

    set_model_thinking_file(&path, "local/small", "max").expect("insert value");
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        replaced.replace(
            "[providers.local.models.small]\n",
            "[providers.local.models.small]\nthinking = \"max\"\n"
        )
    );
}

#[test]
fn set_model_creates_a_table_and_preview_commits_a_redacted_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), CONFIG);

    let created = preview(
        &path,
        &ModelChange::SetModel {
            model: "local/new".into(),
            thinking: Some("medium".into()),
        },
    )
    .expect("preview");

    assert!(updated_text(&created).contains("[providers.local.models.new]\nthinking = \"medium\""));
    assert_eq!(created.metadata.operation, "set model");
    assert_eq!(created.metadata.target, "local/new");
    assert!(!created.redacted_diff.contains("medium"));
    commit_model_change(&path, &created).expect("commit");
    let reloaded = load_required(&path).expect("reload");
    assert_eq!(reloaded.providers["local"].models["new"].thinking, "medium");
}

#[test]
fn set_model_field_reports_the_field_and_rejects_an_invalid_thinking_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), CONFIG);

    let edited = preview(
        &path,
        &ModelChange::SetModelField {
            model: "local/big".into(),
            field: ModelField::Thinking,
            value: "high".into(),
        },
    )
    .expect("preview");
    assert_eq!(edited.metadata.field, Some("thinking"));
    assert!(updated_text(&edited).contains("thinking = \"high\" # cheap"));
    assert!(!edited.redacted_diff.contains("high"));

    let invalid = preview(
        &path,
        &ModelChange::SetModelField {
            model: "local/big".into(),
            field: ModelField::Thinking,
            value: "extreme".into(),
        },
    )
    .expect_err("invalid thinking");
    assert!(
        invalid.to_string().contains("invalid thinking"),
        "{invalid}"
    );
}

#[test]
fn remove_model_deletes_the_table_unless_a_setting_selects_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), CONFIG);

    let removed = preview(
        &path,
        &ModelChange::RemoveModel {
            model: "local/small".into(),
        },
    )
    .expect("remove unselected model");
    assert!(!updated_text(&removed).contains("models.small"));
    assert!(updated_text(&removed).contains("models.big"));

    let selected = preview(
        &path,
        &ModelChange::RemoveModel {
            model: "local/big".into(),
        },
    )
    .expect_err("default model");
    assert!(
        matches!(selected, ModelChangeError::InUse(_)),
        "{selected:?}"
    );
    assert!(selected.to_string().contains("default_model"), "{selected}");

    let reviewer = format!("{CONFIG}\n[approval]\nreviewer_model = \"local/small\"\n");
    let path = write_config(dir.path(), &reviewer);
    let selected = preview(
        &path,
        &ModelChange::RemoveModel {
            model: "local/small".into(),
        },
    )
    .expect_err("reviewer model");
    assert!(
        matches!(selected, ModelChangeError::InUse(_)),
        "{selected:?}"
    );
    assert!(
        selected.to_string().contains("approval.reviewer_model"),
        "{selected}"
    );
}

#[test]
fn remove_provider_is_refused_while_a_setting_selects_one_of_its_models() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), CONFIG);
    let in_use = preview(
        &path,
        &ModelChange::RemoveProvider {
            provider: "local".into(),
        },
    )
    .expect_err("default model uses the provider");
    assert!(matches!(in_use, ModelChangeError::InUse(_)), "{in_use:?}");

    let other = format!("{CONFIG}\n[providers.spare]\ntype = \"chatgpt\"\n");
    let path = write_config(dir.path(), &other);
    let removed = preview(
        &path,
        &ModelChange::RemoveProvider {
            provider: "spare".into(),
        },
    )
    .expect("unused provider");
    assert!(!updated_text(&removed).contains("spare"));
    assert!(updated_text(&removed).contains("[providers.local]"));
}

#[test]
fn set_provider_validates_names_types_and_environment_variable_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), CONFIG);
    let provider =
        |name: &str, provider_type, api_key_env: Option<&str>| ModelChange::SetProvider {
            provider: name.into(),
            provider_type,
            base_url: Some("https://other.example/v1".into()),
            api_key_env: api_key_env.map(str::to_string),
            api_version: None,
            workspace_id_env: None,
        };

    let created = preview(
        &path,
        &provider("other", ProviderType::OpenAiCompatible, Some("OTHER_KEY")),
    )
    .expect("new provider");
    let text = updated_text(&created);
    let parsed = otto_core::config::parse(&text).expect("parse");
    assert_eq!(parsed.providers["other"].type_, "openai-compatible");
    assert_eq!(parsed.providers["other"].api_key_env, "OTHER_KEY");
    assert!(parsed.providers.contains_key("local"), "{text}");
    assert_eq!(created.metadata.target, "other");

    let invalid_name = preview(
        &path,
        &provider("bad.name", ProviderType::OpenAiCompatible, None),
    )
    .expect_err("name with a dot");
    assert!(
        invalid_name.to_string().contains("provider name"),
        "{invalid_name}"
    );
    let invalid_env = preview(
        &path,
        &provider("other", ProviderType::OpenAiCompatible, Some("not a name")),
    )
    .expect_err("bad environment name");
    assert!(
        invalid_env.to_string().contains("api_key_env"),
        "{invalid_env}"
    );
    let mismatch = preview(&path, &provider("chatgpt", ProviderType::Claude, None))
        .expect_err("built-in type mismatch");
    assert!(mismatch.to_string().contains("chatgpt"), "{mismatch}");
}

#[test]
fn set_provider_on_an_existing_provider_keeps_its_model_tables() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), CONFIG);

    let replaced = preview(
        &path,
        &ModelChange::SetProvider {
            provider: "local".into(),
            provider_type: ProviderType::OpenAiCompatible,
            base_url: Some("https://moved.example/v1".into()),
            api_key_env: None,
            api_version: None,
            workspace_id_env: None,
        },
    )
    .expect("replace connection");

    let file = otto_core::config::parse(&updated_text(&replaced)).expect("parse");
    let provider = &file.providers["local"];
    assert_eq!(provider.base_url, "https://moved.example/v1");
    assert!(provider.api_key_env.is_empty());
    assert!(provider.models.contains_key("big") && provider.models.contains_key("small"));
}

#[test]
fn provider_field_edits_apply_type_rules_and_work_without_a_provider_header() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(
        dir.path(),
        "default_model = \"claude/m\"\n\n[providers.claude.models.m]\n",
    );
    let set = |field, value: &str| ModelChange::SetProviderField {
        provider: "claude".into(),
        field,
        value: value.into(),
    };

    let versioned = preview(&path, &set(ProviderField::ApiVersion, "2026-10-21"))
        .expect("built-in claude without a header");
    let file = otto_core::config::parse(&updated_text(&versioned)).expect("parse");
    assert_eq!(file.providers["claude"].api_version, "2026-10-21");
    assert!(file.providers["claude"].models.contains_key("m"));
    assert_eq!(versioned.metadata.field, Some("api_version"));

    let bad_version =
        preview(&path, &set(ProviderField::ApiVersion, "bad-version")).expect_err("not a date");
    assert!(
        bad_version.to_string().contains("YYYY-MM-DD"),
        "{bad_version}"
    );

    let chatgpt = |field| ModelChange::SetProviderField {
        provider: "chatgpt".into(),
        field,
        value: "https://example.test/v1".into(),
    };
    let rejected = preview(&path, &chatgpt(ProviderField::BaseUrl)).expect_err("chatgpt base url");
    assert!(
        rejected.to_string().contains("chatgpt does not support"),
        "{rejected}"
    );
}
