//! Conversion of the deprecated `[profiles.<name>]` tables and `default_profile`
//! to providers and models.
//!
//! The conversion runs once on the merged [`File`] and on every parsed file.
//! It never overrides a value that the new tables already hold, and it
//! records one diagnostic that contains the equivalent new TOML.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use super::layered::Diagnostic;
use super::model_selector::ModelSelector;
use super::providers::{ModelEntry, ProviderConnection, ProviderEntry, is_builtin_provider};
use super::resolve::normalize_base_url;
use super::{ConfigError, File, PROVIDER_CHATGPT, PROVIDER_OPENAI_COMPATIBLE, go_quote};

/// One deprecated `[profiles.<name>]` table.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyProfile {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub model: String,
    /// Sub-agent model IDs, ordered from cheapest to strongest by the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagent_models: Vec<String>,
    #[serde(default)]
    pub thinking: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub api_version: String,
    #[serde(default)]
    pub workspace_id_env: String,
    pub context_window: Option<i64>,
    pub compaction_window: Option<i64>,
}

fn approval_has(table: &Table, key: &str) -> bool {
    table
        .get("approval")
        .and_then(Value::as_table)
        .is_some_and(|approval| approval.contains_key(key))
}

/// Fails when one source holds both legacy (`profiles`, `default_profile`,
/// `approval.reviewer_profile`) and new (`providers`, `default_model`,
/// `approval.reviewer_model`) model keys.
pub(super) fn reject_mixed_source(table: &Table, source: &str) -> Result<(), ConfigError> {
    let legacy = ["profiles", "default_profile"]
        .into_iter()
        .find(|key| table.contains_key(*key))
        .map(str::to_string)
        .or_else(|| {
            approval_has(table, "reviewer_profile").then(|| "approval.reviewer_profile".to_string())
        });
    let current = ["providers", "default_model"]
        .into_iter()
        .find(|key| table.contains_key(*key))
        .map(str::to_string)
        .or_else(|| {
            approval_has(table, "reviewer_model").then(|| "approval.reviewer_model".to_string())
        });
    match (legacy, current) {
        (Some(legacy), Some(current)) => Err(ConfigError::new(format!(
            "{source} mixes the deprecated key `{legacy}` with `{current}`; use only providers, \
             default_model and approval.reviewer_model, or only the deprecated profile keys"
        ))),
        _ => Ok(()),
    }
}

/// A profile that can become a model.
struct ConvertibleProfile {
    name: String,
    provider_type: String,
    entry: ProviderEntry,
    connection: ProviderConnection,
    model_id: String,
    model: ModelEntry,
}

fn classify(name: &str, profile: &LegacyProfile) -> Result<ConvertibleProfile, String> {
    let provider_type = profile.provider.as_str();
    if provider_type.is_empty() || profile.model.is_empty() {
        return Err("provider and model are required".into());
    }
    if !is_builtin_provider(provider_type) {
        return Err(format!("unsupported provider \"{provider_type}\""));
    }
    // Fields that had no effect for the type are dropped, as the new format rejects them.
    let mut entry = ProviderEntry {
        type_: provider_type.into(),
        base_url: profile.base_url.clone(),
        api_key_env: profile.api_key_env.clone(),
        api_version: profile.api_version.clone(),
        workspace_id_env: profile.workspace_id_env.clone(),
        models: BTreeMap::new(),
    };
    match provider_type {
        PROVIDER_CHATGPT => {
            entry.base_url.clear();
            entry.api_key_env.clear();
            entry.api_version.clear();
            entry.workspace_id_env.clear();
        }
        PROVIDER_OPENAI_COMPATIBLE => {
            entry.api_version.clear();
            entry.workspace_id_env.clear();
        }
        _ => {}
    }
    let mut connection = entry.connection("");
    if let Ok(normalized) = normalize_base_url(&connection.base_url) {
        connection.base_url = normalized;
    }
    Ok(ConvertibleProfile {
        name: name.into(),
        provider_type: provider_type.into(),
        entry,
        connection,
        model_id: profile.model.clone(),
        model: ModelEntry {
            thinking: profile.thinking.clone(),
            context_window: profile.context_window,
            compaction_window: profile.compaction_window,
            legacy_subagent_models: profile.subagent_models.clone(),
        },
    })
}

fn builtin_connection(provider_type: &str) -> ProviderConnection {
    let mut connection = ProviderEntry::default().connection(provider_type);
    if let Ok(normalized) = normalize_base_url(&connection.base_url) {
        connection.base_url = normalized;
    }
    connection
}

fn sanitized_provider_name(profile_name: &str) -> String {
    let name: String = profile_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        "profile".into()
    } else {
        name
    }
}

/// Converts `file.profiles`, `file.default_profile` and
/// `file.approval.reviewer_profile` to the provider and model tables.
pub fn convert_legacy_profiles(file: &mut File) -> Result<(), ConfigError> {
    if file.profiles.is_empty()
        && file.default_profile.is_empty()
        && file.approval.reviewer_profile.is_none()
    {
        return Ok(());
    }
    let profiles = std::mem::take(&mut file.profiles);
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut convertible: Vec<ConvertibleProfile> = Vec::new();
    for (name, profile) in &profiles {
        match classify(name, profile) {
            Ok(converted) => convertible.push(converted),
            Err(reason) => skipped.push((name.clone(), reason)),
        }
    }

    // Provider name for each profile: the built-in provider, or a shared custom provider.
    let mut taken: BTreeSet<String> = file.providers.keys().cloned().collect();
    taken.extend(
        super::providers::BUILTIN_PROVIDERS
            .iter()
            .map(|name| name.to_string()),
    );
    let mut custom_groups: Vec<(ProviderConnection, String, ProviderEntry)> = Vec::new();
    let mut provider_of: BTreeMap<String, String> = BTreeMap::new();
    for profile in &convertible {
        if profile.connection == builtin_connection(&profile.provider_type) {
            provider_of.insert(profile.name.clone(), profile.provider_type.clone());
            continue;
        }
        if let Some((_, name, _)) = custom_groups
            .iter()
            .find(|(key, _, _)| *key == profile.connection)
        {
            provider_of.insert(profile.name.clone(), name.clone());
            continue;
        }
        let mut name = sanitized_provider_name(&profile.name);
        while taken.contains(&name) {
            name = format!("{name}-custom");
        }
        taken.insert(name.clone());
        provider_of.insert(profile.name.clone(), name.clone());
        custom_groups.push((profile.connection.clone(), name, profile.entry.clone()));
    }

    // Models; two profiles with the same selector must agree.
    let mut converted_models: BTreeMap<ModelSelector, (String, ModelEntry)> = BTreeMap::new();
    let mut selectors: BTreeMap<String, ModelSelector> = BTreeMap::new();
    for profile in &convertible {
        let selector = ModelSelector {
            provider: provider_of[&profile.name].clone(),
            model_id: profile.model_id.clone(),
        };
        if let Some((first_profile, existing)) = converted_models.get(&selector) {
            if *existing != profile.model {
                return Err(ConfigError::new(format!(
                    "profiles \"{first_profile}\" and \"{}\" both become model \"{selector}\" with different \
                     thinking, context_window, compaction_window or subagent_models; remove one of them \
                     or make their settings equal",
                    profile.name
                )));
            }
        } else {
            converted_models.insert(
                selector.clone(),
                (profile.name.clone(), profile.model.clone()),
            );
        }
        selectors.insert(profile.name.clone(), selector);
    }

    for (_, name, entry) in &custom_groups {
        file.providers.insert(name.clone(), entry.clone());
    }
    for (selector, (_, model)) in &converted_models {
        file.providers
            .entry(selector.provider.clone())
            .or_default()
            .models
            .entry(selector.model_id.clone())
            .or_insert_with(|| model.clone());
    }

    let mut derived_default = None;
    if !file.default_profile.is_empty() {
        if let Some(selector) = selectors.get(&file.default_profile) {
            if file.default_model.is_empty() {
                file.default_model = selector.to_string();
                derived_default = Some(selector.to_string());
            }
            file.default_profile.clear();
        } else if !file.default_model.is_empty() {
            file.default_profile.clear();
        }
    }
    let mut derived_reviewer = None;
    if let Some(reviewer) = file.approval.reviewer_profile.clone() {
        if let Some(selector) = selectors.get(&reviewer) {
            if file.approval.reviewer_model.is_none() {
                file.approval.reviewer_model = Some(selector.to_string());
                derived_reviewer = Some(selector.to_string());
            }
            file.approval.reviewer_profile = None;
        } else if file.approval.reviewer_model.is_some() {
            file.approval.reviewer_profile = None;
        }
    }
    file.legacy_profile_selectors.extend(selectors);

    if !profiles.is_empty() || derived_default.is_some() || derived_reviewer.is_some() {
        let equivalent = equivalent_toml(
            derived_default.as_deref(),
            derived_reviewer.as_deref(),
            &custom_groups,
            &converted_models,
            &skipped,
            convertible
                .iter()
                .any(|profile| !profile.model.legacy_subagent_models.is_empty()),
        );
        file.diagnostics.push(Diagnostic {
            key: "profiles".into(),
            source: "configuration".into(),
            reason: format!(
                "[profiles.*] and default_profile are deprecated; the equivalent configuration is:\n{equivalent}"
            ),
        });
    }
    Ok(())
}

fn push_field(out: &mut String, key: &str, value: &str) {
    if !value.is_empty() {
        out.push_str(&format!("{key} = {}\n", go_quote(value)));
    }
}

/// The inline table of one converted model: `{}` when it sets nothing, else
/// `{ thinking = "high", context_window = 1 }`.
fn inline_model(model: &ModelEntry) -> String {
    let mut fields = Vec::new();
    if !model.thinking.is_empty() {
        fields.push(format!("thinking = {}", go_quote(&model.thinking)));
    }
    for (key, value) in [
        ("context_window", model.context_window),
        ("compaction_window", model.compaction_window),
    ] {
        if let Some(value) = value {
            fields.push(format!("{key} = {value}"));
        }
    }
    if fields.is_empty() {
        "{}".to_string()
    } else {
        format!("{{ {} }}", fields.join(", "))
    }
}

fn equivalent_toml(
    default_model: Option<&str>,
    reviewer_model: Option<&str>,
    custom_groups: &[(ProviderConnection, String, ProviderEntry)],
    models: &BTreeMap<ModelSelector, (String, ModelEntry)>,
    skipped: &[(String, String)],
    has_subagent_models: bool,
) -> String {
    let mut out = String::new();
    if let Some(selector) = default_model {
        push_field(&mut out, "default_model", selector);
        out.push('\n');
    }
    if let Some(selector) = reviewer_model {
        out.push_str("[approval]\n");
        push_field(&mut out, "reviewer_model", selector);
        out.push('\n');
    }
    let mut provider_names: Vec<&str> = custom_groups
        .iter()
        .map(|(_, name, _)| name.as_str())
        .collect();
    for selector in models.keys() {
        if !provider_names.contains(&selector.provider.as_str()) {
            provider_names.push(&selector.provider);
        }
    }
    for name in provider_names {
        out.push_str(&format!("[providers.{name}]\n"));
        match custom_groups.iter().find(|(_, custom, _)| custom == name) {
            Some((_, _, entry)) => {
                push_field(&mut out, "type", &entry.type_);
                push_field(&mut out, "base_url", &entry.base_url);
                push_field(&mut out, "api_key_env", &entry.api_key_env);
                push_field(&mut out, "api_version", &entry.api_version);
                push_field(&mut out, "workspace_id_env", &entry.workspace_id_env);
            }
            None => push_field(&mut out, "type", name),
        }
        for (selector, (_, model)) in models {
            if selector.provider == name {
                out.push_str(&format!(
                    "models.{} = {}\n",
                    go_quote(&selector.model_id),
                    inline_model(model)
                ));
            }
        }
        out.push('\n');
    }
    for (name, reason) in skipped {
        out.push_str(&format!("# profiles.{name} was not converted: {reason}\n"));
    }
    if has_subagent_models {
        out.push_str("# subagent_models is deprecated and has no equivalent in the new format\n");
    }
    out.trim_end().to_string() + "\n"
}
