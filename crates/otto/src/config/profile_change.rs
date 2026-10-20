//! Typed profile changes: validation, preview and commit.

use super::*;

/// catch-all string variant: profile management never accepts other providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProfileProvider {
    OpenAiCompatible,
    ChatGpt,
}

impl ProfileProvider {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => PROVIDER_OPENAI_COMPATIBLE,
            Self::ChatGpt => PROVIDER_CHATGPT,
        }
    }
}

/// One editable, non-secret profile field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProfileField {
    Model,
    Thinking,
    BaseUrl,
    ApiKeyEnv,
}

impl ProfileField {
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Thinking => "thinking",
            Self::BaseUrl => "base_url",
            Self::ApiKeyEnv => "api_key_env",
        }
    }
}

/// A typed, non-secret change to the profile portion of `config.toml`.
///
/// `api_key_env` is an environment-variable *name*, never an API-key value.
/// There is deliberately no generic TOML edit operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProfileChange {
    SetDefault {
        profile: String,
    },
    Create {
        profile: String,
        provider: ProfileProvider,
        model: String,
        thinking: String,
        base_url: Option<String>,
        api_key_env: Option<String>,
    },
    SetField {
        profile: String,
        field: ProfileField,
        value: String,
    },
    Remove {
        profile: String,
    },
}

/// Safe, human-readable information about a profile change. It intentionally
/// contains no configured values (including the API-key environment name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileChangeMetadata {
    pub operation: &'static str,
    pub profile: String,
    pub field: Option<ProfileField>,
}

/// The result a UI/server presents before confirmation.
///
/// `original` is the exact file snapshot read for the preview; `updated` is
/// the exact replacement proposed from it. `commit_profile_change` uses these
/// bytes with the normal compare-and-swap writer, so a stale preview cannot
/// overwrite another change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileChangePreview {
    pub original: Vec<u8>,
    pub updated: Vec<u8>,
    pub metadata: ProfileChangeMetadata,
    /// A value-redacted description suitable for direct display.
    pub redacted_diff: String,
}

/// Previews a typed profile change without writing it.
pub fn preview_profile_change(
    path: &Path,
    change: &ProfileChange,
) -> Result<ProfileChangePreview, NativeConfigError> {
    preview_profile_change_impl(path, &default_path(), change)
}

pub(super) fn preview_profile_change_impl(
    path: &Path,
    default_path: &Path,
    change: &ProfileChange,
) -> Result<ProfileChangePreview, NativeConfigError> {
    let (original, file) = load_with_bytes(path, default_path)?;
    let content = String::from_utf8_lossy(&original);
    let (updated, metadata) = match change {
        ProfileChange::SetDefault { profile } => {
            validate_profile_name(profile)?;
            let profile_config = file
                .profiles
                .get(profile)
                .ok_or_else(|| ConfigError::new(format!("profile {profile:?} not found")))?;
            validate_provider(&profile_config.provider)?;
            (
                otto_core::config::set_default_profile(&content, profile)?,
                ProfileChangeMetadata {
                    operation: "set default profile",
                    profile: profile.clone(),
                    field: None,
                },
            )
        }
        ProfileChange::Create {
            profile,
            provider,
            model,
            thinking,
            base_url,
            api_key_env,
        } => {
            validate_profile_name(profile)?;
            if file.profiles.contains_key(profile) {
                return Err(ConfigError::new(format!("profile {profile:?} already exists")).into());
            }
            validate_model(model)?;
            validate_thinking(thinking)?;
            validate_create_fields(*provider, base_url.as_deref(), api_key_env.as_deref())?;
            let mut table = toml::Table::new();
            table.insert(
                "provider".into(),
                toml::Value::String(provider.as_str().into()),
            );
            table.insert("model".into(), toml::Value::String(model.clone()));
            if !thinking.is_empty() {
                table.insert("thinking".into(), toml::Value::String(thinking.clone()));
            }
            if let Some(base_url) = base_url {
                table.insert("base_url".into(), toml::Value::String(base_url.clone()));
            }
            if let Some(api_key_env) = api_key_env {
                table.insert(
                    "api_key_env".into(),
                    toml::Value::String(api_key_env.clone()),
                );
            }
            (
                otto_core::config::edit::insert_table(&content, &["profiles", profile], table)?,
                ProfileChangeMetadata {
                    operation: "create profile",
                    profile: profile.clone(),
                    field: None,
                },
            )
        }
        ProfileChange::SetField {
            profile,
            field,
            value,
        } => {
            validate_profile_name(profile)?;
            let profile_config = file
                .profiles
                .get(profile)
                .ok_or_else(|| ConfigError::new(format!("profile {profile:?} not found")))?;
            validate_field(*field, value, &profile_config.provider)?;
            let value = toml::Value::String(value.clone()).to_string();
            (
                otto_core::config::edit::set_value(
                    &content,
                    &["profiles", profile],
                    field.key(),
                    Some(&value),
                )?,
                ProfileChangeMetadata {
                    operation: "set profile field",
                    profile: profile.clone(),
                    field: Some(*field),
                },
            )
        }
        ProfileChange::Remove { profile } => {
            validate_profile_name(profile)?;
            let profile_config = file
                .profiles
                .get(profile)
                .ok_or_else(|| ConfigError::new(format!("profile {profile:?} not found")))?;
            validate_provider(&profile_config.provider)?;
            if file.profiles.len() == 1 {
                return Err(ConfigError::new("cannot remove the last profile").into());
            }
            if file.default_profile == *profile {
                return Err(ConfigError::new("cannot remove the default profile").into());
            }
            (
                otto_core::config::edit::remove_table(&content, &["profiles", profile])?,
                ProfileChangeMetadata {
                    operation: "remove profile",
                    profile: profile.clone(),
                    field: None,
                },
            )
        }
    };
    let redacted_diff = match metadata.field {
        Some(field) => format!(
            "{} {:?} for profile {:?}: <redacted>",
            metadata.operation, field, metadata.profile
        ),
        None => format!("{} {:?}", metadata.operation, metadata.profile),
    };
    Ok(ProfileChangePreview {
        original,
        updated: updated.into_bytes(),
        metadata,
        redacted_diff,
    })
}

/// Commits a previously generated preview through the normal atomic,
/// CAS-backed configuration writer.
pub fn commit_profile_change(
    path: &Path,
    preview: &ProfileChangePreview,
) -> Result<(), NativeConfigError> {
    write_bytes(path, &preview.original, &preview.updated).map_err(io_error)
}

pub(super) fn validate_profile_name(name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some(ch) if ch.is_ascii_alphanumeric())
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err(ConfigError::new(
            "invalid profile name: use ASCII letters, digits, _ or -, starting with a letter or digit",
        ));
    }
    Ok(())
}

pub(super) fn validate_model(model: &str) -> Result<(), ConfigError> {
    if model.is_empty() {
        Err(ConfigError::new("missing model"))
    } else {
        Ok(())
    }
}

pub(super) fn validate_thinking(thinking: &str) -> Result<(), ConfigError> {
    if matches!(thinking, "" | "low" | "medium" | "high" | "xhigh" | "max") {
        Ok(())
    } else {
        Err(ConfigError::new(
            "invalid thinking: must be one of low, medium, high, xhigh, max",
        ))
    }
}

pub(super) fn validate_env_name(name: &str) -> Result<(), ConfigError> {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(ConfigError::new(
            "invalid api_key_env: use an environment variable name",
        ));
    }
    Ok(())
}

pub(super) fn validate_create_fields(
    provider: ProfileProvider,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
) -> Result<(), ConfigError> {
    match provider {
        ProfileProvider::OpenAiCompatible => {
            if base_url.is_none_or(str::is_empty) {
                return Err(ConfigError::new("missing base_url"));
            }
            if let Some(name) = api_key_env {
                validate_env_name(name)?;
            }
        }
        ProfileProvider::ChatGpt if base_url.is_some() || api_key_env.is_some() => {
            return Err(ConfigError::new(
                "chatgpt does not support base_url or api_key_env",
            ));
        }
        ProfileProvider::ChatGpt => {}
    }
    Ok(())
}

pub(super) fn validate_provider(provider: &str) -> Result<(), ConfigError> {
    if matches!(provider, PROVIDER_OPENAI_COMPATIBLE | PROVIDER_CHATGPT) {
        Ok(())
    } else {
        Err(ConfigError::new(format!(
            "unsupported provider {provider:?}"
        )))
    }
}

pub(super) fn validate_field(
    field: ProfileField,
    value: &str,
    provider: &str,
) -> Result<(), ConfigError> {
    validate_provider(provider)?;
    match field {
        ProfileField::Model => validate_model(value),
        ProfileField::Thinking => validate_thinking(value),
        ProfileField::BaseUrl => {
            if provider == PROVIDER_CHATGPT {
                Err(ConfigError::new(
                    "chatgpt does not support base_url or api_key_env",
                ))
            } else if value.is_empty() {
                Err(ConfigError::new("missing base_url"))
            } else {
                Ok(())
            }
        }
        ProfileField::ApiKeyEnv => {
            if provider == PROVIDER_CHATGPT {
                Err(ConfigError::new(
                    "chatgpt does not support base_url or api_key_env",
                ))
            } else {
                validate_env_name(value)
            }
        }
    }
}
