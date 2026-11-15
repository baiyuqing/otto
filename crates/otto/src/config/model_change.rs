//! Typed provider and model changes: validation, preview and commit.

use super::*;
use otto_core::config::ModelSelector;
use otto_core::config::model_selector::validate_provider_name;

/// The API type of a provider; management never accepts other types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderType {
    OpenAiCompatible,
    Claude,
    ChatGpt,
}

impl ProviderType {
    fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => PROVIDER_OPENAI_COMPATIBLE,
            Self::Claude => PROVIDER_CLAUDE,
            Self::ChatGpt => PROVIDER_CHATGPT,
        }
    }
}

/// One editable, non-secret provider connection field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProviderField {
    BaseUrl,
    ApiKeyEnv,
    ApiVersion,
    WorkspaceIdEnv,
}

impl ProviderField {
    pub fn key(self) -> &'static str {
        match self {
            Self::BaseUrl => "base_url",
            Self::ApiKeyEnv => "api_key_env",
            Self::ApiVersion => "api_version",
            Self::WorkspaceIdEnv => "workspace_id_env",
        }
    }
}

/// One editable model field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModelField {
    Thinking,
}

impl ModelField {
    pub fn key(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
        }
    }
}

/// A typed, non-secret change to the provider and model tables of `config.toml`.
///
/// `api_key_env` is an environment-variable *name*, never an API-key value.
/// There is deliberately no generic TOML edit operation. `model` is a
/// `<provider>/<model_id>` selector.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModelChange {
    SetDefaultModel {
        model: String,
    },
    SetProvider {
        provider: String,
        provider_type: ProviderType,
        base_url: Option<String>,
        api_key_env: Option<String>,
        api_version: Option<String>,
        workspace_id_env: Option<String>,
    },
    SetProviderField {
        provider: String,
        field: ProviderField,
        value: String,
    },
    RemoveProvider {
        provider: String,
    },
    SetModel {
        model: String,
        thinking: Option<String>,
    },
    SetModelField {
        model: String,
        field: ModelField,
        value: String,
    },
    RemoveModel {
        model: String,
    },
}

/// Why a change was rejected.
#[derive(Debug, thiserror::Error)]
pub enum ModelChangeError {
    /// The provider or model is selected by `default_model` or
    /// `approval.reviewer_model`.
    #[error("{0}")]
    InUse(String),
    #[error(transparent)]
    Native(#[from] NativeConfigError),
}

impl From<ConfigError> for ModelChangeError {
    fn from(error: ConfigError) -> Self {
        Self::Native(error.into())
    }
}

/// Safe, human-readable information about a change. It intentionally contains
/// no configured values (including the API-key environment name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChangeMetadata {
    pub operation: &'static str,
    /// The provider name, or the model selector.
    pub target: String,
    pub field: Option<&'static str>,
}

/// The result a UI/server presents before confirmation.
///
/// `original` is the exact file snapshot read for the preview; `updated` is
/// the exact replacement proposed from it. `commit_model_change` uses these
/// bytes with the normal compare-and-swap writer, so a stale preview cannot
/// overwrite another change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChangePreview {
    pub original: Vec<u8>,
    pub updated: Vec<u8>,
    pub metadata: ModelChangeMetadata,
    /// A value-redacted description suitable for direct display.
    pub redacted_diff: String,
}

/// Previews a typed change without writing it.
pub fn preview_model_change(
    path: &Path,
    change: &ModelChange,
) -> Result<ModelChangePreview, ModelChangeError> {
    preview_model_change_impl(path, &default_path(), change)
}

pub(super) fn preview_model_change_impl(
    path: &Path,
    default_path: &Path,
    change: &ModelChange,
) -> Result<ModelChangePreview, ModelChangeError> {
    let (original, file) = load_with_bytes(path, default_path)?;
    let content = String::from_utf8_lossy(&original);
    let (updated, metadata) = apply(&content, &file, change)?;
    // The edit functions prove the text edit is exact; parsing proves the
    // result is a valid Otto configuration (types, fields, selectors).
    otto_core::config::parse(&updated)?;
    let redacted_diff = match metadata.field {
        Some(field) => format!(
            "{} {field} of {:?}: <redacted>",
            metadata.operation, metadata.target
        ),
        None => format!("{} {:?}", metadata.operation, metadata.target),
    };
    Ok(ModelChangePreview {
        original,
        updated: updated.into_bytes(),
        metadata,
        redacted_diff,
    })
}

fn apply(
    content: &str,
    file: &File,
    change: &ModelChange,
) -> Result<(String, ModelChangeMetadata), ModelChangeError> {
    let metadata = |operation, target: &str, field| ModelChangeMetadata {
        operation,
        target: target.to_string(),
        field,
    };
    match change {
        ModelChange::SetDefaultModel { model } => {
            let selector = existing_provider_selector(file, model)?;
            Ok((
                otto_core::config::set_default_model(content, model)?,
                metadata("set default model", &selector.to_string(), None),
            ))
        }
        ModelChange::SetProvider {
            provider,
            provider_type,
            base_url,
            api_key_env,
            api_version,
            workspace_id_env,
        } => {
            validate_name(provider)?;
            let mut body = toml::Table::new();
            body.insert("type".into(), provider_type.as_str().into());
            let optional = [
                ("base_url", base_url),
                ("api_key_env", api_key_env),
                ("api_version", api_version),
                ("workspace_id_env", workspace_id_env),
            ];
            for (key, value) in optional {
                if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
                    body.insert(key.into(), value.into());
                }
            }
            for (key, value) in [
                ("api_key_env", api_key_env),
                ("workspace_id_env", workspace_id_env),
            ] {
                if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
                    validate_env_name(key, value)?;
                }
            }
            let updated = if file.providers.contains_key(provider) {
                replace_connection(content, provider, body)?
            } else {
                otto_core::config::edit::insert_table(content, &["providers", provider], body)?
            };
            Ok((updated, metadata("set provider", provider, None)))
        }
        ModelChange::SetProviderField {
            provider,
            field,
            value,
        } => {
            validate_name(provider)?;
            let entry = file
                .provider_entry(provider)
                .ok_or_else(|| ConfigError::new(format!("provider {provider:?} not found")))?;
            validate_provider_field(*field, value, entry.provider_type(provider))?;
            let updated = set_provider_value(content, file, provider, field.key(), value)?;
            Ok((
                updated,
                metadata("set provider field", provider, Some(field.key())),
            ))
        }
        ModelChange::RemoveProvider { provider } => {
            validate_name(provider)?;
            if !file.providers.contains_key(provider) {
                return Err(ConfigError::new(format!(
                    "provider {provider:?} has no configuration to remove"
                ))
                .into());
            }
            for (setting, selected) in selected_models(file) {
                if selected.provider == *provider {
                    return Err(ModelChangeError::InUse(format!(
                        "provider {provider:?} is in use: {setting} selects {selected}"
                    )));
                }
            }
            Ok((
                otto_core::config::edit::remove_table(content, &["providers", provider])?,
                metadata("remove provider", provider, None),
            ))
        }
        ModelChange::SetModel { model, thinking } => {
            let selector = existing_provider_selector(file, model)?;
            let thinking = thinking.as_deref().unwrap_or_default();
            validate_thinking(thinking)?;
            let exists = file
                .providers
                .get(&selector.provider)
                .is_some_and(|entry| entry.models.contains_key(&selector.model_id));
            let path = model_path(&selector);
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            let updated = if exists {
                set_thinking(content, &path, thinking)?
            } else {
                let mut body = toml::Table::new();
                if !thinking.is_empty() {
                    body.insert("thinking".into(), thinking.into());
                }
                add_model_entry(content, &selector.provider, &selector.model_id, body)?
            };
            Ok((updated, metadata("set model", &selector.to_string(), None)))
        }
        ModelChange::SetModelField {
            model,
            field,
            value,
        } => {
            let selector = existing_provider_selector(file, model)?;
            validate_thinking(value)?;
            let exists = file
                .providers
                .get(&selector.provider)
                .is_some_and(|entry| entry.models.contains_key(&selector.model_id));
            let path = model_path(&selector);
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            let updated = if exists {
                set_thinking(content, &path, value)?
            } else {
                let mut body = toml::Table::new();
                if !value.is_empty() {
                    body.insert(field.key().into(), value.as_str().into());
                }
                add_model_entry(content, &selector.provider, &selector.model_id, body)?
            };
            Ok((
                updated,
                metadata("set model field", &selector.to_string(), Some(field.key())),
            ))
        }
        ModelChange::RemoveModel { model } => {
            let selector = existing_provider_selector(file, model)?;
            let exists = file
                .providers
                .get(&selector.provider)
                .is_some_and(|entry| entry.models.contains_key(&selector.model_id));
            if !exists {
                return Err(
                    ConfigError::new(format!("model {selector} has no table to remove")).into(),
                );
            }
            for (setting, selected) in selected_models(file) {
                if selected == selector {
                    return Err(ModelChangeError::InUse(format!(
                        "model {selector} is in use: {setting} selects it"
                    )));
                }
            }
            let path = model_path(&selector);
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            let updated = if otto_core::config::edit::has_header(content, &path) {
                otto_core::config::edit::remove_table(content, &path)?
            } else {
                otto_core::config::edit::set_inline_table(content, &path[..2], &path[2..], None)?
            };
            Ok((
                updated,
                metadata("remove model", &selector.to_string(), None),
            ))
        }
    }
}

/// The selectors that `default_model` and `approval.reviewer_model` hold, with
/// the setting name. Values that do not parse are skipped.
fn selected_models(file: &File) -> Vec<(&'static str, ModelSelector)> {
    let mut selected = Vec::new();
    if let Ok(selector) = ModelSelector::parse(&file.default_model) {
        selected.push(("default_model", selector));
    }
    if let Some(Ok(selector)) = file
        .approval
        .reviewer_model
        .as_deref()
        .map(ModelSelector::parse)
    {
        selected.push(("approval.reviewer_model", selector));
    }
    selected
}

fn existing_provider_selector(file: &File, text: &str) -> Result<ModelSelector, ConfigError> {
    let selector = ModelSelector::parse(text)?;
    if file.provider_entry(&selector.provider).is_none() {
        return Err(ConfigError::new(format!(
            "provider {:?} not found",
            selector.provider
        )));
    }
    Ok(selector)
}

fn model_path(selector: &ModelSelector) -> [String; 4] {
    [
        "providers".into(),
        selector.provider.clone(),
        "models".into(),
        selector.model_id.clone(),
    ]
}

/// Writes a new model as `models."<id>" = { ... }` in `[providers.<name>]`,
/// creating that header (without a `type`) when absent. A provider whose
/// models are already written as `[providers.<name>.models."<id>"]` tables
/// gets the new model in that form, so all models of one provider keep one
/// spelling and one place in the file; a provider with inline models or no
/// models gets the inline form. The header form is also the fallback when the
/// inline text does not parse to the original plus the model.
pub fn add_model_entry(
    content: &str,
    provider: &str,
    model_id: &str,
    body: toml::Table,
) -> Result<String, ConfigError> {
    let path = ["providers", provider, "models", model_id];
    if otto_core::config::edit::has_header_below(content, &path[..3]) {
        return otto_core::config::edit::insert_table(content, &path, body);
    }
    match otto_core::config::edit::set_inline_table(
        content,
        &path[..2],
        &path[2..],
        Some(body.clone()),
    ) {
        Ok(updated) => Ok(updated),
        Err(_) => otto_core::config::edit::insert_table(content, &path, body),
    }
}

fn set_thinking(content: &str, path: &[&str], thinking: &str) -> Result<String, ConfigError> {
    let value = (!thinking.is_empty()).then(|| toml::Value::String(thinking.into()).to_string());
    otto_core::config::edit::set_value(content, path, "thinking", value.as_deref())
}

/// Sets one provider key. An existing `[providers.<name>]` header is edited in
/// place; a provider with no header of its own (a built-in without overrides,
/// possibly with `[providers.<name>.models.<id>]` tables) gets a new header
/// that holds the key.
fn set_provider_value(
    content: &str,
    file: &File,
    provider: &str,
    key: &str,
    value: &str,
) -> Result<String, ConfigError> {
    let value_text = (!value.is_empty()).then(|| toml::Value::String(value.into()).to_string());
    let mut body = toml::Table::new();
    if !value.is_empty() {
        body.insert(key.into(), value.into());
    }
    if !file.providers.contains_key(provider) {
        return otto_core::config::edit::insert_table(content, &["providers", provider], body);
    }
    match otto_core::config::edit::set_value(
        content,
        &["providers", provider],
        key,
        value_text.as_deref(),
    ) {
        Ok(updated) => Ok(updated),
        // No `[providers.<name>]` header: the key is absent, so removing it is
        // a no-op and setting it appends the header.
        Err(_) if value.is_empty() => Ok(content.to_string()),
        Err(error) => append_provider_header(content, provider, body).map_err(|_| error),
    }
}

/// Replaces every connection field of an existing provider with `body`; model
/// tables stay.
fn replace_connection(
    content: &str,
    provider: &str,
    body: toml::Table,
) -> Result<String, ConfigError> {
    let mut updated = content.to_string();
    for key in [
        "type",
        "base_url",
        "api_key_env",
        "api_version",
        "workspace_id_env",
    ] {
        let value = body.get(key).map(toml::Value::to_string);
        updated = match otto_core::config::edit::set_value(
            &updated,
            &["providers", provider],
            key,
            value.as_deref(),
        ) {
            Ok(next) => next,
            Err(_) if value.is_none() => updated,
            Err(error) => {
                // Headerless provider: append one header carrying all keys.
                return append_provider_header(content, provider, body).map_err(|_| error);
            }
        };
    }
    Ok(updated)
}

/// Appends `[providers.<name>]` with `body` after the last byte of `content`.
/// TOML allows the header after `[providers.<name>.models.<id>]` tables that
/// created the provider implicitly; the result must parse to the original
/// document plus exactly `body` under the provider.
fn append_provider_header(
    content: &str,
    provider: &str,
    body: toml::Table,
) -> Result<String, ConfigError> {
    let mut expected: toml::Table =
        toml::from_str(content).map_err(|error| ConfigError::new(error.to_string()))?;
    let table = expected
        .entry("providers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .and_then(|providers| providers.get_mut(provider))
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| ConfigError::new(format!("provider {provider:?} cannot be edited")))?;
    for (key, value) in &body {
        table.insert(key.clone(), value.clone());
    }
    let mut providers = toml::Table::new();
    providers.insert(provider.to_string(), toml::Value::Table(body));
    let mut root = toml::Table::new();
    root.insert("providers".into(), toml::Value::Table(providers));
    let rendered = toml::to_string(&root).map_err(|error| ConfigError::new(error.to_string()))?;
    let separator = if content.is_empty() || content.ends_with("\n\n") {
        ""
    } else if content.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let updated = format!("{content}{separator}{rendered}");
    let reparsed: toml::Table =
        toml::from_str(&updated).map_err(|error| ConfigError::new(error.to_string()))?;
    if reparsed != expected {
        return Err(ConfigError::new(format!(
            "provider {provider:?} cannot be edited without changing other settings"
        )));
    }
    Ok(updated)
}

/// Commits a previously generated preview through the normal atomic,
/// CAS-backed configuration writer.
pub(crate) fn commit_model_change(
    path: &Path,
    preview: &ModelChangePreview,
) -> Result<(), NativeConfigError> {
    write_bytes(path, &preview.original, &preview.updated).map_err(io_error)
}

fn validate_name(name: &str) -> Result<(), ConfigError> {
    validate_provider_name(name).map_err(ConfigError::new)
}

fn validate_thinking(thinking: &str) -> Result<(), ConfigError> {
    if matches!(thinking, "" | "low" | "medium" | "high" | "xhigh" | "max") {
        Ok(())
    } else {
        Err(ConfigError::new(
            "invalid thinking: must be one of low, medium, high, xhigh, max",
        ))
    }
}

fn validate_env_name(field: &str, name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(ConfigError::new(format!(
            "invalid {field}: use an environment variable name"
        )));
    }
    Ok(())
}

fn validate_provider_field(
    field: ProviderField,
    value: &str,
    provider_type: &str,
) -> Result<(), ConfigError> {
    let is_claude = provider_type == PROVIDER_CLAUDE;
    match field {
        ProviderField::ApiVersion => {
            let bytes = value.as_bytes();
            let is_date = bytes.len() == 10
                && bytes.iter().enumerate().all(|(index, byte)| {
                    if index == 4 || index == 7 {
                        *byte == b'-'
                    } else {
                        byte.is_ascii_digit()
                    }
                });
            if !is_claude || !is_date {
                return Err(ConfigError::new(
                    "api_version requires Claude and a YYYY-MM-DD API version",
                ));
            }
        }
        ProviderField::WorkspaceIdEnv => {
            if !is_claude {
                return Err(ConfigError::new("workspace_id_env requires Claude"));
            }
            if !value.is_empty() {
                validate_env_name("workspace_id_env", value)?;
            }
        }
        ProviderField::BaseUrl => {
            if provider_type == PROVIDER_CHATGPT {
                return Err(ConfigError::new(
                    "chatgpt does not support base_url or api_key_env",
                ));
            }
            if value.is_empty() {
                return Err(ConfigError::new("missing base_url"));
            }
            otto_core::config::endpoint::normalize_base_url(value).map_err(ConfigError::new)?;
        }
        ProviderField::ApiKeyEnv => {
            if provider_type == PROVIDER_CHATGPT {
                return Err(ConfigError::new(
                    "chatgpt does not support base_url or api_key_env",
                ));
            }
            validate_env_name("api_key_env", value)?;
        }
    }
    Ok(())
}
