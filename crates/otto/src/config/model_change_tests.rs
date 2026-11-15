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
        "https://api.openai.com/v1",
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
    assert_eq!(provider.base_url, "https://api.openai.com/v1");
    assert!(provider.models.contains_key("cheap-model"));
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

#[test]
fn a_bad_thinking_value_is_rejected_at_preview_time() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = write_config(directory.path(), CONFIG);
    let change = ModelChange::SetModel {
        model: "local/big".into(),
        thinking: Some("extreme".into()),
    };
    let error = preview(&path, &change).expect_err("invalid thinking");
    assert!(error.to_string().contains("invalid thinking"), "{error}");
}

const INLINE_CONFIG: &str = "# retained\ndefault_model = \"local/big\"\n\n\
    [providers.local]\ntype = \"openai-compatible\"\nbase_url = \"https://example.test/v1\"\n\
    models.\"big\" = { thinking = \"low\" } # cheap\nmodels.\"small\" = {}\n\n\
    [ui]\nmode = \"repl\"\n";

fn model_change_text(content: &str, change: ModelChange) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), content);
    updated_text(&preview(&path, &change).expect("preview"))
}

#[test]
fn new_model_in_a_provider_with_a_header_adds_one_inline_line_in_its_block() {
    let updated = model_change_text(
        INLINE_CONFIG,
        ModelChange::SetModel {
            model: "local/new".into(),
            thinking: None,
        },
    );
    assert_eq!(
        updated,
        INLINE_CONFIG.replace(
            "models.\"small\" = {}\n",
            "models.\"small\" = {}\nmodels.\"new\" = {}\n"
        )
    );
    let with_thinking = model_change_text(
        INLINE_CONFIG,
        ModelChange::SetModel {
            model: "local/new".into(),
            thinking: Some("high".into()),
        },
    );
    assert!(
        with_thinking.contains("models.\"new\" = { thinking = \"high\" }\n"),
        "{with_thinking}"
    );
}

#[test]
fn new_model_for_a_built_in_provider_without_a_header_appends_the_header_and_one_line() {
    let content = "default_model = \"local/big\"\n\n[providers.local]\ntype = \"openai-compatible\"\n\
                   base_url = \"https://example.test/v1\"\nmodels.\"big\" = {}\n";
    let updated = model_change_text(
        content,
        ModelChange::SetModel {
            model: "chatgpt/gpt-5.6-sol".into(),
            thinking: None,
        },
    );
    assert_eq!(
        updated,
        format!("{content}\n[providers.chatgpt]\nmodels.\"gpt-5.6-sol\" = {{}}\n")
    );
}

#[test]
fn new_model_for_a_provider_with_only_header_style_tables_is_written_as_a_header_table() {
    let content = "[providers.chatgpt.models.\"a\"]\nthinking = \"low\"\n";
    let updated = model_change_text(
        content,
        ModelChange::SetModel {
            model: "chatgpt/b".into(),
            thinking: Some("high".into()),
        },
    );
    assert!(
        updated.contains("[providers.chatgpt.models.b]\nthinking = \"high\"\n"),
        "{updated}"
    );
    let parsed = otto_core::config::parse(&updated).expect("parse");
    assert_eq!(parsed.providers["chatgpt"].models["b"].thinking, "high");
    assert_eq!(parsed.providers["chatgpt"].models["a"].thinking, "low");
}

#[test]
fn set_thinking_and_remove_edit_an_inline_model_where_it_is_written() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), INLINE_CONFIG);

    set_model_thinking_file(&path, "local/big", "high").expect("replace");
    set_model_thinking_file(&path, "local/small", "max").expect("insert");
    let edited = fs::read_to_string(&path).expect("read back");
    assert!(
        edited.contains("models.\"big\" = { thinking = \"high\" } # cheap\n"),
        "{edited}"
    );
    assert!(
        edited.contains("models.\"small\" = { thinking = \"max\" }\n"),
        "{edited}"
    );
    set_model_thinking_file(&path, "local/small", "").expect("clear");
    assert_eq!(
        fs::read_to_string(&path).expect("read back"),
        edited.replace("{ thinking = \"max\" }", "{}")
    );

    let removed = model_change_text(
        INLINE_CONFIG,
        ModelChange::RemoveModel {
            model: "local/small".into(),
        },
    );
    assert_eq!(
        removed,
        INLINE_CONFIG.replace("models.\"small\" = {}\n", "")
    );
}

#[test]
fn set_thinking_and_remove_still_edit_a_header_style_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_config(dir.path(), CONFIG);
    set_model_thinking_file(&path, "local/small", "max").expect("set");
    assert!(
        fs::read_to_string(&path)
            .expect("read")
            .contains("[providers.local.models.small]\nthinking = \"max\"\n")
    );
    let removed = model_change_text(
        CONFIG,
        ModelChange::RemoveModel {
            model: "local/small".into(),
        },
    );
    assert!(!removed.contains("models.small"), "{removed}");
    assert!(
        removed.contains("[providers.local.models.big]"),
        "{removed}"
    );
}

#[test]
fn initial_config_names_the_provider_and_writes_the_model_as_an_inline_entry() {
    assert_eq!(
        initial_config_content(PROVIDER_CHATGPT, "gpt-5.6-sol", "", "").expect("chatgpt"),
        "default_model = \"chatgpt/gpt-5.6-sol\"\n\n[providers.chatgpt]\ntype = \"chatgpt\"\n\
         models.\"gpt-5.6-sol\" = {}\n"
    );
    assert_eq!(
        initial_config_content(
            PROVIDER_OPENAI_COMPATIBLE,
            "m",
            "https://api.openai.com/v1",
            "K"
        )
        .expect("openai-compatible"),
        "default_model = \"openai-compatible/m\"\n\n[providers.openai-compatible]\n\
         type = \"openai-compatible\"\nbase_url = \"https://api.openai.com/v1\"\n\
         api_key_env = \"K\"\nmodels.\"m\" = {}\n"
    );
}

#[test]
fn endpoint_setup_custom_origin_requires_independent_user_configuration() {
    let error = initial_config_content(
        PROVIDER_OPENAI_COMPATIBLE,
        "fixture-model",
        "https://custom.example/v1",
        "FIXTURE_KEY",
    )
    .unwrap_err();
    assert!(error.contains("trusted_origins"));
    assert!(error.contains("getting-started.md#endpoint-trust"));
    assert!(!error.contains("custom.example"));
}
